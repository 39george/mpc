use mpc::manager::Application;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::default())
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(Level::INFO.into())
                .add_directive("axum::rejection=trace".parse().unwrap())
                .add_directive("tower_sessions_core=warn".parse().unwrap())
                .add_directive("sitetrace_axum=info".parse().unwrap())
                .add_directive("aws_config=warn".parse().unwrap()),
        )
        .compact()
        .with_level(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set up tracing");

    Application::build_server()
        .await?
        .run_until_stopped()
        .await?;
    Ok(())
}
