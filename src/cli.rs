use std::io::{BufRead, Write};

use crate::backend::{Backend, Cancellation, EjectResult};
use crate::model::format_capacity;

pub fn run<I, S, B, R, O, E>(
    args: I,
    backend: &mut B,
    mut input: R,
    mut output: O,
    mut error: E,
) -> i32
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
    B: Backend,
    R: BufRead,
    O: Write,
    E: Write,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("list") if args.len() == 2 => list(backend, &mut output, &mut error),
        Some("eject") if args.len() == 3 => {
            eject(&args[2], backend, &mut input, &mut output, &mut error)
        }
        _ => {
            let _ = writeln!(
                error,
                "Usage: usb-ejector list | usb-ejector eject <exact-device-id>"
            );
            2
        }
    }
}

fn list<B: Backend, O: Write, E: Write>(backend: &mut B, output: &mut O, error: &mut E) -> i32 {
    let devices = match backend.list_devices() {
        Ok(devices) => devices,
        Err(problem) => {
            let _ = writeln!(error, "Inventory failed: {problem}");
            return 1;
        }
    };
    if devices.is_empty() {
        let _ = writeln!(output, "No supported USB storage devices were found.");
        return 0;
    }
    for item in devices {
        let physical_capacity = item
            .device
            .capacity_bytes()
            .map(|bytes| format!("physical: {}", format_capacity(bytes)))
            .unwrap_or_else(|| "physical: multiple disks (not summed)".into());
        let volumes = item
            .device
            .volumes
            .iter()
            .map(|volume| {
                let mounts = if volume.mount_paths.is_empty() {
                    "no mount path".into()
                } else {
                    volume.mount_paths.join(", ")
                };
                let capacity = volume
                    .capacity_bytes
                    .map(format_capacity)
                    .unwrap_or_else(|| "unknown size".into());
                format!("{mounts} [{capacity} logical volume]")
            })
            .collect::<Vec<_>>();
        let status = match item.eligibility {
            Ok(()) => "eligible".to_owned(),
            Err(reason) => format!("blocked: {reason}"),
        };
        let _ = writeln!(
            output,
            "{} | {} | {} | {} partition(s) | {} | {}",
            item.device.public_id,
            item.device.name,
            physical_capacity,
            item.device.partition_count(),
            if volumes.is_empty() {
                "no volumes".into()
            } else {
                volumes.join("; ")
            },
            status
        );
    }
    0
}

fn eject<B: Backend, R: BufRead, O: Write, E: Write>(
    id: &str,
    backend: &mut B,
    input: &mut R,
    output: &mut O,
    error: &mut E,
) -> i32 {
    let devices = match backend.list_devices() {
        Ok(devices) => devices,
        Err(problem) => {
            let _ = writeln!(error, "Inventory failed: {problem}");
            return 1;
        }
    };
    let matches = devices
        .into_iter()
        .filter(|item| item.device.public_id == id)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        let _ = writeln!(error, "No single device matches the exact ID {id}.");
        return 1;
    }
    let selected = &matches[0];
    if let Err(reason) = &selected.eligibility {
        let _ = writeln!(error, "Safe removal is blocked: {reason}.");
        return 1;
    }

    let _ = writeln!(output, "{}", selected.device.impact_summary());
    let _ = write!(output, "Type EJECT to request safe removal: ");
    let _ = output.flush();
    let mut confirmation = String::new();
    if input.read_line(&mut confirmation).is_err() || confirmation.trim_end() != "EJECT" {
        let _ = writeln!(output, "Cancelled. No removal request was sent.");
        return 2;
    }

    let exit_code = match backend.request_eject(&selected.device, &Cancellation::new()) {
        Ok(EjectResult::Ejected) => {
            let _ = writeln!(
                output,
                "Windows reports that safe removal was requested successfully."
            );
            0
        }
        Ok(EjectResult::Rejected(failure)) => {
            let _ = writeln!(error, "{}", failure.user_message());
            1
        }
        Err(problem) => {
            let _ = writeln!(error, "Safe removal did not complete: {problem}");
            1
        }
    };
    if exit_code != 0 {
        if let Some(details) = backend.diagnostics() {
            let _ = writeln!(error, "Operation details:\n{details}");
        }
    }
    exit_code
}
