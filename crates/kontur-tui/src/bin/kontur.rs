use kontur_tui::demo::{run, Demo};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    run(Demo::new()).await
}
