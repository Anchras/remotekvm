//! Dev helper: mint a session JWT for a known user id, for local end-to-end
//! testing without the full WorkOS login.
//!
//!   cargo run -p remotekvm-server --example mint_token -- <user_uuid> <jwt_secret>

fn main() {
    let mut args = std::env::args().skip(1);
    let user_id = args
        .next()
        .expect("usage: mint_token <user_uuid> <jwt_secret>");
    let secret = args
        .next()
        .expect("usage: mint_token <user_uuid> <jwt_secret>");
    let token = remotekvm_server::auth::create_token(
        &user_id,
        "workos_dev",
        "dev@example.com",
        &secret,
        24,
    )
    .expect("failed to mint token");
    println!("{token}");
}
