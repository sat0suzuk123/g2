# g2 — bulk-rename files using a CSV mapping table

## Install

    cargo install --path .     # installs to ~/.cargo/bin/g2

## Usage

    g2 -c map.csv [PATHS...]          # preview (changes nothing)
    g2 -c map.csv [PATHS...] -x       # execute
    g2 -u                             # preview undo of the last run
    g2 -u -x                          # undo the last run

`map.csv` has two columns: `search,replacement`. An empty replacement deletes the match.
Files with a `.tsv` extension are read as tab-separated.

    draft-07,chapter-03-final
    draft-12,chapter-08-final

## Options

| Option | Description |
|---|---|
| `-c, --csv FILE` | Mapping CSV |
| `-x, --exec` | Actually rename (default is preview only) |
| `--exact` (default) | Replace a key only when it is not adjacent to alphanumeric characters (`draft-07` does not match `draft-07X`) |
| `--partial` | Replace a key anywhere in the file name |
| `-i, --ignore-case` | Case-insensitive matching |
| `-e, --ignore-ext` | Do not modify file extensions |
| `-R, --recursive` | Recurse into subdirectories |
| `-H, --hidden` | Include hidden files |
| `-d, --include-dir` | Rename directories as well as files |
| `-D, --only-dir` | Rename directories only |
| `-a, --all` | Also show unchanged files |
| `--header` | Skip the first CSV row as a header |
| `--delimiter C` | CSV delimiter |
| `-u, --undo` | Undo the last run |

## Behavior

- Replacement is done in a single pass, preferring longer keys. Replaced text is never matched again by another rule, so row order in the CSV does not matter.
- If any target already exists, or two files would get the same name, nothing is renamed.
- Swaps such as `a -> b`, `b -> a` are handled.
- Directories and the files inside them can be renamed in the same run.
- Each run is logged to `.g2_undo.csv` in the current directory; run `g2 -u -x` from the same directory to revert.
- In `--exact` mode, non-ASCII letters (e.g. CJK characters) count as alphanumeric. Use `--partial` if a key is directly adjacent to such characters.
