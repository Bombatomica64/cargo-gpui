//! `unpatch <old> <delta> <new>`: what `zstd -d --patch-from=<old> <delta> -o <new>`
//! does, for phones without a zstd binary. `hotpatch reload` runs it as the
//! shell user in /data/local/tmp. The frame checksum fails if <old> is not the
//! library the delta was made from.

use std::{fs::File, io, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [old, delta, new] = args.as_slice() else {
        eprintln!("usage: unpatch <old> <delta> <new>");
        return ExitCode::from(2);
    };
    match unpatch(old, delta, new) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("unpatch: {err}");
            _ = std::fs::remove_file(new);
            ExitCode::FAILURE
        }
    }
}

fn unpatch(old: &str, delta: &str, new: &str) -> io::Result<()> {
    let old = std::fs::read(old)?;
    let mut decoder = zstd::stream::read::Decoder::with_ref_prefix(io::BufReader::new(File::open(delta)?), &old)?;
    // --patch-from sets the window to cover the whole old file.
    decoder.window_log_max(31)?;
    io::copy(&mut decoder, &mut io::BufWriter::new(File::create(new)?))?;
    Ok(())
}
