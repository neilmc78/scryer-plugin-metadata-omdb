//! Embed the plugin descriptor in the built component.
//!
//! Scryer's loader identifies a WASI Preview 2 component by the descriptor in
//! its top-level `scryer.plugin-descriptor.v1` custom section, not by calling
//! `describe`, so a release artifact must carry it.
//!
//! Usage: `embed-descriptor <component.wasm> <output.wasm>`

use std::process::ExitCode;

const SECTION_NAME: &str = "scryer.plugin-descriptor.v1";
const COMPONENT_HEADER: &[u8] = b"\0asm\r\0\x01\0";

fn write_u32_leb(mut value: u32, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn embed(component: &[u8], payload: &[u8]) -> Result<Vec<u8>, String> {
    if !component.starts_with(COMPONENT_HEADER) {
        return Err("the input is not a WebAssembly component".to_string());
    }
    if component
        .windows(SECTION_NAME.len())
        .any(|window| window == SECTION_NAME.as_bytes())
    {
        return Err("the component already carries a descriptor section".to_string());
    }
    let name = SECTION_NAME.as_bytes();
    let mut body = Vec::with_capacity(name.len() + payload.len() + 5);
    write_u32_leb(name.len() as u32, &mut body);
    body.extend_from_slice(name);
    body.extend_from_slice(payload);

    let mut out = component.to_vec();
    out.push(0);
    write_u32_leb(
        u32::try_from(body.len()).map_err(|_| "the descriptor is too large".to_string())?,
        &mut out,
    );
    out.extend_from_slice(&body);
    Ok(out)
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output), None) = (args.next(), args.next(), args.next()) else {
        return Err("usage: embed-descriptor <component.wasm> <output.wasm>".to_string());
    };
    let component = std::fs::read(&input).map_err(|error| format!("read {input}: {error}"))?;
    let payload = serde_json::to_vec(&omdb_core::descriptor())
        .map_err(|error| format!("serialize the descriptor: {error}"))?;
    let embedded = embed(&component, &payload)?;
    std::fs::write(&output, embedded).map_err(|error| format!("write {output}: {error}"))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("embed-descriptor: {error}");
            ExitCode::FAILURE
        }
    }
}
