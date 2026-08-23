//! Render the installer for a quick shell check: `cargo run -q -p walgit-server --example render_install [repo]`.
fn main() {
    let mut cfg = walgit_config::Config::default();
    cfg.server.auth.mode = walgit_config::AuthMode::Oidc;
    cfg.server.auth.session_secret = Some("0123456789abcdef0123456789abcdef-example".into());
    cfg.server.auth.allowed_domains = vec!["example.com".into()];
    let repo = std::env::args().nth(1);
    let base = std::env::var("BASE_URL").unwrap_or_else(|_| "https://git.example.com".into());
    print!(
        "{}",
        walgit_server::setup::install_script(&cfg, &base, repo.as_deref())
    );
}
