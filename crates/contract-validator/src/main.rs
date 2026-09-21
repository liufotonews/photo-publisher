use anyhow::{bail, Result};
use photo_publisher_contract_validator::validate;
use std::env;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 {
        bail!("usage: contract-validator <schema.json> <document.json>");
    }

    validate(&args[1], &args[2])?;
    println!("valid");
    Ok(())
}
