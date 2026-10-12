//! `unpatch <old> <delta> <new>`: what `zstd -d --patch-from=<old> <delta> -o <new>`
//! does, for phones without a zstd binary. `cargo gpui` runs it as the shell
//! user in /data/local/tmp. The frame checksum fails if <old> is not the
//! library the delta was made from.
//!
//! `unpatch <old> - <new> [<file>]` reads the delta from stdin instead, so it
//! arrives in the same adb call: a little-endian u64 length, the delta, then
//! the contents of <file> (a patch table), written after <new> so it lands
//! last.

use std::{
    fs::{self, File, Permissions},
    io::{self, Read, Write},
    os::unix::fs::PermissionsExt,
    process::ExitCode,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [old, delta, new] if delta != "-" => File::open(delta).and_then(|d| unpatch(old, d, new)),
        [old, dash, new, rest @ ..] if dash == "-" && rest.len() <= 1 => receive(old, new, rest.first()),
        _ => {
            eprintln!("usage: unpatch <old> <delta> <new> | unpatch <old> - <new> [<file>]");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("unpatch: {err}");
            _ = fs::remove_file(&args[2]);
            ExitCode::FAILURE
        }
    }
}

fn receive(old: &str, new: &str, file: Option<&String>) -> io::Result<()> {
    let mut input = vec![];
    io::stdin().read_to_end(&mut input)?;
    let truncated = || io::Error::new(io::ErrorKind::UnexpectedEof, "truncated input");
    let (len, rest) = input.split_first_chunk::<8>().ok_or_else(truncated)?;
    let (delta, contents) = rest
        .split_at_checked(u64::from_le_bytes(*len) as usize)
        .ok_or_else(truncated)?;
    unpatch(old, delta, new)?;
    if let Some(file) = file {
        // The app polls for the file: write it whole, then rename.
        let tmp = format!("{file}.tmp");
        fs::write(&tmp, contents)?;
        fs::set_permissions(&tmp, Permissions::from_mode(0o644))?;
        fs::rename(&tmp, file)?;
    }
    Ok(())
}

fn unpatch(old: &str, delta: impl Read, new: &str) -> io::Result<()> {
    let old = fs::read(old)?;
    let mut decoder = zstd::stream::read::Decoder::with_ref_prefix(io::BufReader::new(delta), &old)?;
    // --patch-from sets the window to cover the whole old file.
    decoder.window_log_max(31)?;
    let mut out = io::BufWriter::new(File::create(new)?);
    io::copy(&mut decoder, &mut out)?;
    out.flush()?;
    // The app reads it as another user.
    fs::set_permissions(new, Permissions::from_mode(0o644))
}
