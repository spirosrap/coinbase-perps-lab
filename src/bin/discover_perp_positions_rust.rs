use anyhow::Result;
use clap::Parser;
use coinbase_perps_lab::{load_output, render_cli_output};

#[derive(Parser, Debug)]
#[command(about = "Discover open Coinbase INTX perpetual positions with derived market/risk context.")]
struct Args {
    #[arg(long, help = "Optional explicit INTX portfolio UUID")]
    portfolio: Option<String>,
    #[arg(long, help = "Print machine-readable JSON")]
    json: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let output = load_output(args.portfolio.as_deref())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{}", render_cli_output(&output));
    }

    Ok(())
}
