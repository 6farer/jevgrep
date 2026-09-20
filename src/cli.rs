use clap::{ArgAction, Parser};

/// jevgrep: grep (ripgrep-flavoured) where the pattern is a natural-language
/// query judged by the TypeSafe "System One" API.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "jevgrep",
    version,
    about,
    disable_help_flag = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Natural-language description of the lines to find.
    pub query: String,

    /// Files or directories to search. No path: read stdin if it is not a
    /// terminal, otherwise search `.`. A path of `-` means stdin.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,

    // ---- Search scope ----
    /// Only search files matching GLOB (repeatable; `!GLOB` excludes).
    #[arg(short = 'g', long, value_name = "GLOB", help_heading = "Search scope")]
    pub glob: Vec<String>,

    /// Search hidden files/dirs.
    #[arg(long, help_heading = "Search scope")]
    pub hidden: bool,

    /// Don't respect .gitignore/.ignore.
    #[arg(long, help_heading = "Search scope")]
    pub no_ignore: bool,

    /// Stop after opening N files (0 = unlimited).
    #[arg(
        long,
        default_value_t = 500,
        value_name = "N",
        help_heading = "Search scope"
    )]
    pub max_files: usize,

    /// Stop after sending N lines to jev (0 = unlimited).
    #[arg(
        long,
        default_value_t = 2000,
        value_name = "N",
        help_heading = "Search scope"
    )]
    pub max_lines: usize,

    /// Lines longer than N bytes are truncated to N before judging.
    #[arg(
        long,
        default_value_t = 1000,
        value_name = "N",
        help_heading = "Search scope"
    )]
    pub max_line_length: usize,

    /// Walk and count only; print files, lines, estimated requests to
    /// stderr; no API calls; exit 0.
    #[arg(long, help_heading = "Search scope")]
    pub dry_run: bool,

    // ---- Judging ----
    /// Match when probabilities.yes >= P.
    #[arg(
        short = 't',
        long,
        default_value_t = 0.5,
        value_name = "P",
        help_heading = "Judging"
    )]
    pub threshold: f64,

    /// Print the lines that do not match.
    #[arg(short = 'v', long, help_heading = "Judging")]
    pub invert_match: bool,

    /// Lines before/after each line sent to jev as context (0 = judge in
    /// isolation).
    #[arg(long, default_value_t = 2, value_name = "N", help_heading = "Judging")]
    pub judge_context: usize,

    /// Max lines per API request.
    #[arg(
        short = 'b',
        long,
        default_value_t = 16,
        value_name = "N",
        help_heading = "Judging"
    )]
    pub batch: usize,

    /// Max in-flight requests.
    #[arg(
        short = 'j',
        long,
        default_value_t = 4,
        value_name = "N",
        help_heading = "Judging"
    )]
    pub concurrency: usize,

    /// Disable the identical-(line+context) verdict cache.
    #[arg(long, help_heading = "Judging")]
    pub no_cache: bool,

    /// Model id.
    #[arg(
        long,
        default_value = "jev-latest",
        value_name = "M",
        help_heading = "Judging"
    )]
    pub model: String,

    /// Per-request timeout.
    #[arg(
        long,
        default_value_t = 30_000,
        value_name = "MS",
        help_heading = "Judging"
    )]
    pub timeout_ms: u64,

    /// Endpoint override; also JEVGREP_API_URL env. Required for tests.
    #[arg(
        long,
        env = "JEVGREP_API_URL",
        default_value = "https://api.typesafe.ai/v1/systemone",
        value_name = "URL",
        help_heading = "Judging"
    )]
    pub api_url: String,

    /// Exit 2 immediately on the first API error instead of warning and
    /// continuing.
    #[arg(long, help_heading = "Judging")]
    pub strict: bool,

    // ---- Output ----
    /// Prefix with line number.
    #[arg(short = 'n', long, help_heading = "Output")]
    pub line_number: bool,

    /// Always prefix with filename.
    #[arg(short = 'H', long, help_heading = "Output")]
    pub with_filename: bool,

    /// Never prefix with filename.
    #[arg(short = 'h', long, help_heading = "Output")]
    pub no_filename: bool,

    /// Print only the names of files with at least one match.
    #[arg(short = 'l', long, help_heading = "Output")]
    pub files_with_matches: bool,

    /// Print only the names of files with no match.
    #[arg(short = 'L', long, help_heading = "Output")]
    pub files_without_match: bool,

    /// Print `file:count` per file (or just `count` when filename is
    /// suppressed).
    #[arg(short = 'c', long, help_heading = "Output")]
    pub count: bool,

    /// Print N lines after each match.
    #[arg(
        short = 'A',
        long,
        default_value_t = 0,
        value_name = "N",
        help_heading = "Output"
    )]
    pub after_context: usize,

    /// Print N lines before each match.
    #[arg(
        short = 'B',
        long,
        default_value_t = 0,
        value_name = "N",
        help_heading = "Output"
    )]
    pub before_context: usize,

    /// Print N lines before and after each match.
    #[arg(
        short = 'C',
        long,
        default_value_t = 0,
        value_name = "N",
        help_heading = "Output"
    )]
    pub context: usize,

    /// Prefix each printed match with `p=0.97 `.
    #[arg(long, help_heading = "Output")]
    pub show_score: bool,

    /// One JSON object per match, no other stdout output.
    #[arg(long, help_heading = "Output")]
    pub json: bool,

    /// Print totals to stderr at exit.
    #[arg(long, help_heading = "Output")]
    pub stats: bool,

    /// Print help.
    #[arg(long, action = ArgAction::Help, help_heading = "Output")]
    pub help: Option<bool>,
}

impl Cli {
    /// Effective before/after output-context line counts, folding `-C` in.
    pub fn output_context(&self) -> (usize, usize) {
        let before = self.before_context.max(self.context);
        let after = self.after_context.max(self.context);
        (before, after)
    }
}
