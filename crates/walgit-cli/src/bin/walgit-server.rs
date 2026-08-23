//! `walgit-server --config walgit.toml` — the standalone server (D39): exactly `walgit serve`,
//! under the name a single-binary deployment expects.
fn main() -> anyhow::Result<()> {
    walgit_cli::main_server()
}
