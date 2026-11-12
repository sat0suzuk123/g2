//! g2 — bulk-rename files using a CSV mapping table (old,new)
//!
//!   g2 -c map.csv [PATHS...]        preview (changes nothing)
//!   g2 -c map.csv [PATHS...] -x     execute
//!   g2 -u                           preview undo of the last run
//!   g2 -u -x                        undo the last run

use clap::Parser;
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const UNDO_FILE: &str = ".g2_undo.csv";

#[derive(Parser, Debug)]
#[command(
    name = "g2",
    version,
    about = "Bulk-rename files using a CSV mapping table (old,new)",
    after_help = "\
Examples:
  g2 -c map.csv                 preview renames in the current directory
  g2 -c map.csv -R data/ -x     rename everything under data/ recursively
  g2 -c map.csv --partial       replace keys anywhere as substrings
  g2 -u -x                      undo the last run"
)]
struct Cli {
    /// Mapping CSV (column 1 = search string, column 2 = replacement; empty column 2 deletes the match)
    #[arg(short = 'c', long = "csv", value_name = "FILE", required_unless_present = "undo")]
    csv: Option<PathBuf>,

    /// Files or directories to process (default: current directory)
    #[arg(value_name = "PATHS")]
    paths: Vec<PathBuf>,

    /// Actually rename (without this flag, only a preview is shown)
    #[arg(short = 'x', long = "exec")]
    exec: bool,

    /// Exact match: replace a key only when it is not adjacent to alphanumeric characters (default)
    #[arg(long = "exact", conflicts_with = "partial")]
    exact: bool,

    /// Partial match: replace a key anywhere in the file name
    #[arg(long = "partial")]
    partial: bool,

    /// Case-insensitive matching
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Do not modify file extensions
    #[arg(short = 'e', long = "ignore-ext")]
    ignore_ext: bool,

    /// Skip the first CSV row as a header
    #[arg(long = "header")]
    header: bool,

    /// CSV delimiter (default: tab for .tsv files, comma otherwise)
    #[arg(long = "delimiter", value_name = "CHAR")]
    delimiter: Option<char>,

    /// Recurse into subdirectories
    #[arg(short = 'R', long = "recursive")]
    recursive: bool,

    /// Include hidden files (names starting with .)
    #[arg(short = 'H', long = "hidden")]
    hidden: bool,

    /// Rename directories as well as files
    #[arg(short = 'd', long = "include-dir")]
    include_dir: bool,

    /// Rename directories only
    #[arg(short = 'D', long = "only-dir")]
    only_dir: bool,

    /// Also show unchanged files
    #[arg(short = 'a', long = "all")]
    show_all: bool,

    /// Undo the last run (combine with -x to execute)
    #[arg(short = 'u', long = "undo")]
    undo: bool,
}

/// Set of replacement rules
struct Mapper {
    re: Regex,
    table: HashMap<String, String>,
    exact: bool,
    ignore_case: bool,
}

impl Mapper {
    fn load(cli: &Cli, path: &Path) -> Result<Self, String> {
        let delim = cli.delimiter.unwrap_or_else(|| {
            match path.extension().and_then(|e| e.to_str()) {
                Some(e) if e.eq_ignore_ascii_case("tsv") => '\t',
                _ => ',',
            }
        });
        if !delim.is_ascii() {
            return Err("delimiter must be a single ASCII character".into());
        }
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(cli.header)
            .delimiter(delim as u8)
            .flexible(true)
            .from_path(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;

        let mut table = HashMap::new();
        let mut keys = Vec::new();
        for (i, rec) in rdr.records().enumerate() {
            let rec = rec.map_err(|e| format!("{}: {e}", path.display()))?;
            let line = i + 1 + cli.header as usize;
            let key = rec.get(0).unwrap_or("").trim_start_matches('\u{feff}').to_string();
            if key.is_empty() {
                continue;
            }
            let val = rec.get(1).unwrap_or("").to_string();
            if val.contains('/') || val.contains('\0') {
                return Err(format!("{}:{line}: replacement must not contain '/': {val}", path.display()));
            }
            let norm = if cli.ignore_case { key.to_lowercase() } else { key.clone() };
            if let Some(prev) = table.get(&norm) {
                if prev != &val {
                    return Err(format!("{}:{line}: duplicate key {key} ({prev} vs {val})", path.display()));
                }
                continue;
            }
            table.insert(norm, val);
            keys.push(key);
        }
        if keys.is_empty() {
            return Err(format!("{}: no replacement rules found", path.display()));
        }
        // Prefer longer keys (so draft-10 is not mistaken for draft-1)
        keys.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
        let pat = keys.iter().map(|k| regex::escape(k)).collect::<Vec<_>>().join("|");
        let re = RegexBuilder::new(&pat)
            .case_insensitive(cli.ignore_case)
            .size_limit(1 << 28)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Mapper { re, table, exact: !cli.partial, ignore_case: cli.ignore_case })
    }

    fn lookup(&self, k: &str) -> &str {
        if self.ignore_case {
            &self.table[&k.to_lowercase()]
        } else {
            &self.table[k]
        }
    }

    /// Replace in a single pass (replaced text is never matched again by another rule)
    fn apply(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        let mut pos = 0;
        while pos <= s.len() {
            let Some(m) = self.find_at(s, pos) else { break };
            out.push_str(&s[last..m.0]);
            out.push_str(self.lookup(&s[m.0..m.1]));
            last = m.1;
            pos = m.1;
        }
        out.push_str(&s[last..]);
        out
    }

    fn find_at(&self, s: &str, mut pos: usize) -> Option<(usize, usize)> {
        while pos <= s.len() {
            let m = self.re.find_at(s, pos)?;
            if !self.exact || (Self::boundary_before(s, m.start()) && Self::boundary_after(s, m.end())) {
                return Some((m.start(), m.end()));
            }
            // Boundary check failed: advance one character and search again
            pos = m.start() + s[m.start()..].chars().next().map_or(1, |c| c.len_utf8());
        }
        None
    }

    fn boundary_before(s: &str, i: usize) -> bool {
        s[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric())
    }
    fn boundary_after(s: &str, i: usize) -> bool {
        s[i..].chars().next().is_none_or(|c| !c.is_alphanumeric())
    }
}

/// A single rename operation
struct Op {
    src: PathBuf,
    dst: PathBuf,
    depth: usize,
    status: Status,
}

#[derive(PartialEq)]
enum Status {
    Ok,
    Unchanged,
    Conflict(String),
}

fn split_ext(name: &str) -> (&str, &str) {
    // A leading dot is not an extension (e.g. .bashrc). For multi-part extensions like .fastq.gz, only the last part counts
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn is_hidden(p: &Path) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') && n != "." && n != "..")
}

fn collect(cli: &Cli) -> Result<Vec<PathBuf>, String> {
    let roots = if cli.paths.is_empty() { vec![PathBuf::from(".")] } else { cli.paths.clone() };
    let mut out = Vec::new();
    for root in roots {
        let meta = fs::symlink_metadata(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        if meta.is_dir() {
            walk(cli, &root, &mut out)?;
        } else {
            out.push(root); // Explicitly given files are always processed
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk(cli: &Cli, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for ent in rd {
        let ent = ent.map_err(|e| e.to_string())?;
        let p = if dir == Path::new(".") { PathBuf::from(ent.file_name()) } else { ent.path() };
        if !cli.hidden && is_hidden(&p) {
            continue;
        }
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if cli.include_dir || cli.only_dir {
                out.push(p.clone());
            }
            if cli.recursive {
                walk(cli, &p, out)?;
            }
        } else if !cli.only_dir {
            out.push(p);
        }
    }
    Ok(())
}

fn plan(cli: &Cli, mapper: &Mapper, paths: Vec<PathBuf>) -> Vec<Op> {
    paths
        .into_iter()
        .filter_map(|src| {
            let name = src.file_name()?.to_str()?.to_string();
            let new = if cli.ignore_ext && !fs::symlink_metadata(&src).map(|m| m.is_dir()).unwrap_or(false) {
                let (stem, ext) = split_ext(&name);
                format!("{}{}", mapper.apply(stem), ext)
            } else {
                mapper.apply(&name)
            };
            let dst = src.with_file_name(&new);
            let depth = src.components().count();
            let status = if new == name { Status::Unchanged } else { Status::Ok };
            Some(Op { src, dst, depth, status })
        })
        .collect()
}

/// Check for conflicts. Returns true if any were found
fn validate(ops: &mut [Op]) -> bool {
    let sources: HashSet<PathBuf> =
        ops.iter().filter(|o| o.status == Status::Ok).map(|o| o.src.clone()).collect();
    let mut bad = false;
    let mut dup_count: HashMap<PathBuf, usize> = HashMap::new();
    for o in ops.iter().filter(|o| o.status == Status::Ok) {
        *dup_count.entry(o.dst.clone()).or_default() += 1;
    }
    for o in ops.iter_mut().filter(|o| o.status == Status::Ok) {
        let name = o.dst.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.is_empty() || name == "." || name == ".." {
            o.status = Status::Conflict("empty or invalid name".into());
        } else if dup_count[&o.dst] > 1 {
            o.status = Status::Conflict("multiple files map to this name".into());
        } else if fs::symlink_metadata(&o.dst).is_ok() && !sources.contains(&o.dst) && !same_file_case_change(&o.src, &o.dst) {
            o.status = Status::Conflict("target already exists".into());
        }
        if matches!(o.status, Status::Conflict(_)) {
            bad = true;
        }
    }
    bad
}

/// Case-only change (same file on case-insensitive file systems)
fn same_file_case_change(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

fn print_table(ops: &[Op], show_all: bool) {
    let rows: Vec<_> = ops.iter().filter(|o| show_all || o.status != Status::Unchanged).collect();
    let w1 = rows.iter().map(|o| width(&o.src.display().to_string())).max().unwrap_or(0).max(8);
    let w2 = rows.iter().map(|o| width(&o.dst.display().to_string())).max().unwrap_or(0).max(7);
    let line = format!("+{}+{}+{}+", "-".repeat(w1 + 2), "-".repeat(w2 + 2), "-".repeat(30));
    println!("{line}");
    println!("| {} | {} | {:<28} |", pad("ORIGINAL", w1), pad("RENAMED", w2), "STATUS");
    println!("{line}");
    for o in rows {
        let st = match &o.status {
            Status::Ok => "ok".to_string(),
            Status::Unchanged => "unchanged".to_string(),
            Status::Conflict(m) => format!("NG: {m}"),
        };
        println!(
            "| {} | {} | {} |",
            pad(&o.src.display().to_string(), w1),
            pad(&o.dst.display().to_string(), w2),
            pad(&st, 28)
        );
    }
    println!("{line}");
}

/// Count wide (CJK) characters as width 2 (simple approximation for table alignment)
fn width(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) < 0x1100 { 1 } else { 2 }).sum()
}
fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

/// Execute. Deepest paths first; within each depth, rename via temporary names in two phases (handles swaps like a<->b)
fn execute(ops: &[Op]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut todo: Vec<&Op> = ops.iter().filter(|o| o.status == Status::Ok).collect();
    todo.sort_by(|a, b| b.depth.cmp(&a.depth));
    let pid = std::process::id();
    let mut done = Vec::new();
    let mut i = 0;
    while i < todo.len() {
        let d = todo[i].depth;
        let level: Vec<&Op> = todo[i..].iter().take_while(|o| o.depth == d).copied().collect();
        i += level.len();
        let mut tmps = Vec::new();
        for (n, o) in level.iter().enumerate() {
            let tmp = o.src.with_file_name(format!(".g2_tmp_{pid}_{n}"));
            fs::rename(&o.src, &tmp).map_err(|e| format!("{}: {e}", o.src.display()))?;
            tmps.push(tmp);
        }
        for (o, tmp) in level.iter().zip(tmps) {
            fs::rename(&tmp, &o.dst).map_err(|e| format!("{} -> {}: {e}", o.src.display(), o.dst.display()))?;
            done.push((o.src.clone(), o.dst.clone()));
        }
    }
    Ok(done)
}

/// Final path after parent directories are renamed (for the undo log)
fn final_path(p: &Path, renamed: &HashMap<PathBuf, PathBuf>) -> PathBuf {
    let Some(name) = p.file_name() else { return p.to_path_buf() };
    let parent = p.parent().unwrap_or(Path::new(""));
    let fparent = if parent.as_os_str().is_empty() { PathBuf::new() } else { final_path(parent, renamed) };
    match renamed.get(p) {
        Some(dst) => fparent.join(dst.file_name().unwrap()),
        None => fparent.join(name),
    }
}

fn write_undo(done: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let renamed: HashMap<PathBuf, PathBuf> = done.iter().cloned().collect();
    let mut w = csv::Writer::from_path(UNDO_FILE).map_err(|e| e.to_string())?;
    w.write_record(["current", "original_name"]).map_err(|e| e.to_string())?;
    for (src, _) in done {
        let cur = std::path::absolute(final_path(src, &renamed)).map_err(|e| e.to_string())?;
        let orig = src.file_name().unwrap().to_string_lossy();
        w.write_record([cur.to_string_lossy().as_ref(), orig.as_ref()]).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())
}

fn load_undo() -> Result<Vec<Op>, String> {
    let mut rdr = csv::Reader::from_path(UNDO_FILE)
        .map_err(|_| format!("{UNDO_FILE} not found (run this in the directory where you ran g2 -x)"))?;
    let mut ops = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        let src = PathBuf::from(&rec[0]);
        let dst = src.with_file_name(&rec[1]);
        let depth = src.components().count();
        ops.push(Op { src, dst, depth, status: Status::Ok });
    }
    Ok(ops)
}

fn run(cli: Cli) -> Result<bool, String> {
    let mut ops = if cli.undo {
        load_undo()?
    } else {
        let mapper = Mapper::load(&cli, cli.csv.as_ref().unwrap())?;
        plan(&cli, &mapper, collect(&cli)?)
    };
    let bad = validate(&mut ops);
    let n_ok = ops.iter().filter(|o| o.status == Status::Ok).count();
    let n_ng = ops.iter().filter(|o| matches!(o.status, Status::Conflict(_))).count();
    if n_ok + n_ng == 0 && !cli.show_all {
        println!("no files matched");
        return Ok(true);
    }
    print_table(&ops, cli.show_all);
    if bad {
        eprintln!("{n_ng} conflict(s) found; nothing was renamed");
        return Ok(false);
    }
    if !cli.exec {
        println!("{n_ok} file(s) would be renamed (preview only; add -x to apply)");
        return Ok(true);
    }
    let done = execute(&ops)?;
    if cli.undo {
        let _ = fs::remove_file(UNDO_FILE);
        println!("{} rename(s) undone", done.len());
    } else {
        write_undo(&done)?;
        println!("{} file(s) renamed (undo with: g2 -u -x)", done.len());
    }
    std::io::stdout().flush().ok();
    Ok(true)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("g2: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper(rows: &[(&str, &str)], partial: bool, ignore_case: bool) -> Mapper {
        let dir = std::env::temp_dir().join(format!("g2test_{}_{}", std::process::id(), rows.len() * 10 + partial as usize));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("m.csv");
        let body: String = rows.iter().map(|(a, b)| format!("{a},{b}\r\n")).collect();
        fs::write(&p, body).unwrap();
        let mut args = vec!["g2", "-c", p.to_str().unwrap()];
        if partial { args.push("--partial"); }
        if ignore_case { args.push("-i"); }
        Mapper::load(&Cli::parse_from(args), &p).unwrap()
    }

    #[test]
    fn exact_respects_boundaries() {
        let m = mapper(&[("draft-07", "chapter-03-final")], false, false);
        assert_eq!(m.apply("draft-07_v2.txt"), "chapter-03-final_v2.txt");
        assert_eq!(m.apply("draft-07X.txt"), "draft-07X.txt");
        assert_eq!(m.apply("xdraft-07.txt"), "xdraft-07.txt");
        assert_eq!(m.apply("draft-07"), "chapter-03-final");
    }

    #[test]
    fn partial_matches_anywhere() {
        let m = mapper(&[("draft-07", "NEW")], true, false);
        assert_eq!(m.apply("draft-07X.txt"), "NEWX.txt");
    }

    #[test]
    fn no_chained_replacement_and_longest_first() {
        let m = mapper(&[("a", "b"), ("b", "c"), ("AD1", "S1"), ("AD10", "S10")], true, false);
        assert_eq!(m.apply("ab"), "bc");
        assert_eq!(m.apply("AD10_AD1"), "S10_S1");
    }

    #[test]
    fn ignore_case_and_multibyte() {
        let m = mapper(&[("株式会社", "(株)"), ("abc", "X")], false, true);
        assert_eq!(m.apply("株式会社_ABC.txt"), "(株)_X.txt");
    }
}
