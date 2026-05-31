use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE, STILL_ACTIVE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken, WTS_CURRENT_SERVER_HANDLE,
    WTS_SESSION_INFOW,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, TerminateProcess, CREATE_UNICODE_ENVIRONMENT,
    NORMAL_PRIORITY_CLASS, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

const SERVICE_NAME: &str = "RemoteKVM Agent";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const SESSION_SCAN_INTERVAL: Duration = Duration::from_secs(10);
const STOP_EXIT_CODE: u32 = 0xC0DE;

static SERVICE_CONFIG: OnceLock<ServiceConfig> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub server_url: String,
    pub registration_token: String,
}

impl ServiceConfig {
    pub fn new(server_url: String, registration_token: String) -> Self {
        Self {
            server_url,
            registration_token,
        }
    }

    fn from_env() -> Result<Self> {
        let server_url = std::env::var("RKVM_SERVER_URL")
            .unwrap_or_else(|_| "ws://localhost:8080/agent".to_owned());
        let registration_token = std::env::var("RKVM_REGISTRATION_TOKEN")
            .context("RKVM_REGISTRATION_TOKEN is required in service mode")?;
        Ok(Self::new(server_url, registration_token))
    }
}

/// Run the agent as a Windows service.
///
/// The service controller:
///   1. Connects to the SaaS server via WebSocket
///   2. Monitors active user sessions via WTS
///   3. Spawns a session agent when a user logs in
///   4. Handles machine registration and heartbeats through ServerClient
pub async fn run_service(config: ServiceConfig) -> Result<()> {
    info!("starting Windows service controller");
    let _ = SERVICE_CONFIG.set(config.clone());

    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(()),
        Err(error) => {
            warn!(
                %error,
                "service dispatcher unavailable; running service controller in foreground mode"
            );
            run_foreground(config).await
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    let config = SERVICE_CONFIG
        .get()
        .cloned()
        .map(Ok)
        .unwrap_or_else(ServiceConfig::from_env);

    match config.and_then(run_service_entry) {
        Ok(()) => {}
        Err(error) => error!(%error, "Windows service controller failed"),
    }
}

fn run_service_entry(config: ServiceConfig) -> Result<()> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let control_tx = event_tx.clone();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = control_tx.send(ServiceEvent::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(param) => {
                let _ = control_tx.send(ServiceEvent::SessionChange {
                    session_id: param.session_id,
                    reason: param.reason,
                });
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("register Windows service control handler")?;

    set_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        Duration::from_secs(20),
    )?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("remotekvm-service")
        .build()
        .context("create Windows service Tokio runtime")?;

    set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        0,
        Duration::default(),
    )?;

    let controller_result = runtime.block_on(run_controller(config, event_rx, false));

    set_status(
        &status_handle,
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        1,
        Duration::from_secs(15),
    )?;

    if let Err(error) = controller_result {
        error!(%error, "service controller loop failed");
    }

    set_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        0,
        Duration::default(),
    )?;

    Ok(())
}

fn set_status(
    status_handle: &windows_service::service_control_handler::ServiceStatusHandle,
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    checkpoint: u32,
    wait_hint: Duration,
) -> Result<()> {
    status_handle
        .set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint,
            wait_hint,
            process_id: None,
        })
        .context("set Windows service status")
}

/// Run the service controller in foreground mode (for development).
async fn run_foreground(config: ServiceConfig) -> Result<()> {
    info!("running service controller in foreground mode");
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    run_controller(config, event_rx, true).await
}

async fn run_controller(
    config: ServiceConfig,
    mut events: mpsc::UnboundedReceiver<ServiceEvent>,
    foreground: bool,
) -> Result<()> {
    let (_server, _requests) =
        crate::server_client::ServerClient::connect(&config.server_url, &config.registration_token)
            .await
            .context("connect service controller to server")?;
    info!("service controller connected to server");

    let mut sessions = SessionAgents::new(config);
    sessions.reconcile_active_sessions()?;

    let mut scan_interval = tokio::time::interval(SESSION_SCAN_INTERVAL);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c(), if foreground => {
                info!("Ctrl+C received, shutting down service controller");
                break;
            }
            event = events.recv() => {
                match event {
                    Some(ServiceEvent::Stop) | None => {
                        info!("service stop requested");
                        break;
                    }
                    Some(ServiceEvent::SessionChange { session_id, reason }) => {
                        sessions.handle_session_change(session_id, reason)?;
                    }
                }
            }
            _ = scan_interval.tick() => {
                sessions.reap_exited();
                sessions.reconcile_active_sessions()?;
            }
        }
    }

    sessions.stop_all();
    info!("shutting down service controller");
    Ok(())
}

#[derive(Debug)]
enum ServiceEvent {
    Stop,
    SessionChange {
        session_id: u32,
        reason: SessionChangeReason,
    },
}

struct SessionAgents {
    config: ServiceConfig,
    agents: HashMap<u32, SessionAgent>,
}

impl SessionAgents {
    fn new(config: ServiceConfig) -> Self {
        Self {
            config,
            agents: HashMap::new(),
        }
    }

    fn handle_session_change(
        &mut self,
        session_id: u32,
        reason: SessionChangeReason,
    ) -> Result<()> {
        match reason {
            SessionChangeReason::ConsoleConnect
            | SessionChangeReason::RemoteConnect
            | SessionChangeReason::SessionLogon
            | SessionChangeReason::SessionUnlock
            | SessionChangeReason::SessionCreate => {
                if active_sessions()?.contains(&session_id) {
                    self.ensure_agent(session_id)?;
                }
            }
            SessionChangeReason::ConsoleDisconnect
            | SessionChangeReason::RemoteDisconnect
            | SessionChangeReason::SessionLogoff
            | SessionChangeReason::SessionTerminate => {
                self.stop_session(session_id);
            }
            SessionChangeReason::SessionLock | SessionChangeReason::SessionRemoteControl => {}
        }
        Ok(())
    }

    fn reconcile_active_sessions(&mut self) -> Result<()> {
        let active = active_sessions()?;
        for session_id in &active {
            self.ensure_agent(*session_id)?;
        }

        let inactive: Vec<u32> = self
            .agents
            .keys()
            .copied()
            .filter(|session_id| !active.contains(session_id))
            .collect();
        for session_id in inactive {
            self.stop_session(session_id);
        }

        Ok(())
    }

    fn ensure_agent(&mut self, session_id: u32) -> Result<()> {
        if session_id == 0 || self.agents.contains_key(&session_id) {
            return Ok(());
        }

        match spawn_session_agent(session_id, &self.config) {
            Ok(agent) => {
                info!(
                    session_id,
                    process_id = agent.process_id,
                    "spawned Windows session agent"
                );
                self.agents.insert(session_id, agent);
            }
            Err(error) => {
                warn!(session_id, %error, "failed to spawn Windows session agent");
            }
        }
        Ok(())
    }

    fn reap_exited(&mut self) {
        self.agents.retain(|session_id, agent| {
            if agent.is_running() {
                true
            } else {
                info!(*session_id, "Windows session agent exited");
                false
            }
        });
    }

    fn stop_session(&mut self, session_id: u32) {
        if let Some(agent) = self.agents.remove(&session_id) {
            info!(
                session_id,
                process_id = agent.process_id,
                "stopping session agent"
            );
            agent.terminate();
        }
    }

    fn stop_all(&mut self) {
        let session_ids: Vec<u32> = self.agents.keys().copied().collect();
        for session_id in session_ids {
            self.stop_session(session_id);
        }
    }
}

struct SessionAgent {
    process: Handle,
    thread: Handle,
    process_id: u32,
}

impl SessionAgent {
    fn is_running(&self) -> bool {
        let mut exit_code = 0;
        let result = unsafe { GetExitCodeProcess(self.process.raw(), &mut exit_code) };
        result.is_ok() && exit_code == STILL_ACTIVE.0 as u32
    }

    fn terminate(self) {
        if self.is_running() {
            if let Err(error) = unsafe { TerminateProcess(self.process.raw(), STOP_EXIT_CODE) } {
                warn!(process_id = self.process_id, %error, "failed to terminate session agent");
            }
        }
    }
}

fn active_sessions() -> Result<HashSet<u32>> {
    let mut sessions_ptr: *mut WTS_SESSION_INFOW = null_mut();
    let mut count = 0;

    unsafe {
        WTSEnumerateSessionsW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut sessions_ptr,
            &mut count,
        )
        .context("enumerate WTS sessions")?;
    }

    let sessions = unsafe { std::slice::from_raw_parts(sessions_ptr, count as usize) };
    let active = sessions
        .iter()
        .filter(|session| session.State == WTSActive && session.SessionId != 0)
        .map(|session| session.SessionId)
        .collect();

    unsafe {
        WTSFreeMemory(sessions_ptr.cast());
    }

    Ok(active)
}

fn spawn_session_agent(session_id: u32, config: &ServiceConfig) -> Result<SessionAgent> {
    let exe = std::env::current_exe().context("resolve current agent executable")?;
    let command_line = session_agent_command_line(&exe, config);
    let mut command_line_w = to_wide_mut(&command_line);
    let desktop = to_wide_mut("winsta0\\default");

    let user_token = query_user_token(session_id)?;
    let primary_token = duplicate_primary_token(user_token.raw())?;
    let environment = EnvironmentBlock::create(primary_token.raw())?;

    let mut startup = STARTUPINFOW::default();
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.lpDesktop = PWSTR(desktop.as_ptr() as *mut _);

    let mut process_info = PROCESS_INFORMATION::default();
    let creation_flags =
        PROCESS_CREATION_FLAGS((NORMAL_PRIORITY_CLASS | CREATE_UNICODE_ENVIRONMENT).0);

    unsafe {
        CreateProcessAsUserW(
            primary_token.raw(),
            PCWSTR::null(),
            PWSTR(command_line_w.as_mut_ptr()),
            None,
            None,
            FALSE,
            creation_flags,
            Some(environment.as_ptr()),
            PCWSTR::null(),
            &startup,
            &mut process_info,
        )
        .with_context(|| format!("create session agent in WTS session {session_id}"))?;
    }

    Ok(SessionAgent {
        process: Handle::new(process_info.hProcess),
        thread: Handle::new(process_info.hThread),
        process_id: process_info.dwProcessId,
    })
}

fn query_user_token(session_id: u32) -> Result<Handle> {
    let mut token = HANDLE::default();
    unsafe {
        WTSQueryUserToken(session_id, &mut token)
            .with_context(|| format!("query user token for WTS session {session_id}"))?;
    }
    Ok(Handle::new(token))
}

fn duplicate_primary_token(token: HANDLE) -> Result<Handle> {
    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
        .context("duplicate session user token")?;
    }
    Ok(Handle::new(primary))
}

fn session_agent_command_line(exe: &PathBuf, config: &ServiceConfig) -> String {
    let exe = exe.as_os_str().to_string_lossy();
    format!(
        "{} --server-url {} --token {}",
        quote_command_arg(&exe),
        quote_command_arg(&config.server_url),
        quote_command_arg(&config.registration_token)
    )
}

fn quote_command_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_owned();
    }

    let needs_quotes = arg
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'));
    if !needs_quotes {
        return arg.to_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat('\\').take(backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
    quoted.push('"');
    quoted
}

fn to_wide_mut(value: &str) -> Vec<u16> {
    OsString::from(value).encode_wide().chain(Some(0)).collect()
}

struct Handle(HANDLE);

impl Handle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            if let Err(error) = unsafe { CloseHandle(self.0) } {
                tracing::debug!(%error, "failed to close Windows handle");
            }
        }
    }
}

struct EnvironmentBlock(*mut std::ffi::c_void);

impl EnvironmentBlock {
    fn create(token: HANDLE) -> Result<Self> {
        let mut environment = null_mut();
        unsafe {
            CreateEnvironmentBlock(&mut environment, token, FALSE)
                .context("create session environment block")?;
        }
        Ok(Self(environment))
    }

    fn as_ptr(&self) -> *const std::ffi::c_void {
        self.0.cast_const()
    }
}

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            if let Err(error) = unsafe { DestroyEnvironmentBlock(self.0) } {
                tracing::debug!(%error, "failed to destroy environment block");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_command_args_for_windows_process_creation() {
        assert_eq!(quote_command_arg("simple"), "simple");
        assert_eq!(quote_command_arg("has space"), "\"has space\"");
        assert_eq!(quote_command_arg("has\"quote"), "\"has\\\"quote\"");
        assert_eq!(
            quote_command_arg(r#"C:\Program Files\App\"#),
            r#""C:\Program Files\App\\""#
        );
    }
}
