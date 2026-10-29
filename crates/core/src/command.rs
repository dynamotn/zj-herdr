//! What a shell command is, from its words alone: a bounded job (build,
//! test, deploy, other), a service that runs until stopped, a connection to
//! another machine, or an interactive program the user sits in.

/// The kind of a shell command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Build,
    Test,
    Deploy,
    /// Runs until stopped: a dev server, a watcher, `tail -f`.
    Service,
    /// `ssh`, `mosh`: a session on another machine.
    Remote,
    /// Editors, pagers, TUIs and REPLs: they wait on the user, not the
    /// other way round.
    Interactive,
    Other,
}

impl Class {
    /// One-cell mark for list views.
    pub fn mark(self) -> &'static str {
        match self {
            Class::Build => "⚙",
            Class::Test => "⚗",
            Class::Deploy => "⇡",
            Class::Service => "❯",
            Class::Remote => "⇄",
            Class::Interactive | Class::Other => "$",
        }
    }

    /// Runs until stopped: shown steady, never as work being waited on.
    pub fn is_steady(self) -> bool {
        matches!(self, Class::Service | Class::Remote)
    }

    /// The `kind` a command row carries, and back.
    pub fn kind(self) -> &'static str {
        match self {
            Class::Build => "build",
            Class::Test => "test",
            Class::Deploy => "deploy",
            Class::Service => "server",
            Class::Remote => "remote",
            Class::Interactive | Class::Other => "command",
        }
    }

    pub fn from_kind(kind: &str) -> Option<Class> {
        [
            Class::Build,
            Class::Test,
            Class::Deploy,
            Class::Service,
            Class::Remote,
            Class::Other,
        ]
        .into_iter()
        .find(|c| c.kind() == kind)
    }
}

/// Program names of interactive shells: a pane running one is at a prompt.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "xonsh", "elvish",
];

const INTERACTIVE: &[&str] = &[
    "nvim",
    "vim",
    "vi",
    "nano",
    "emacs",
    "hx",
    "helix",
    "micro",
    "kak",
    "less",
    "more",
    "most",
    "man",
    "bat",
    "htop",
    "btop",
    "top",
    "atop",
    "glances",
    "lazygit",
    "lazydocker",
    "tig",
    "gitui",
    "yazi",
    "ranger",
    "nnn",
    "lf",
    "broot",
    "fzf",
    "k9s",
    "psql",
    "mysql",
    "sqlite3",
    "redis-cli",
    "watch",
    "zellij",
    "tmux",
];

const REMOTE: &[&str] = &["ssh", "mosh", "et", "autossh"];

/// Words that run the next word as the real command.
const WRAPPERS: &[&str] = &[
    "env",
    "nohup",
    "sudo",
    "doas",
    "time",
    "exec",
    "command",
    "nice",
    "caffeinate",
    "timeout",
];

/// Package runners: the command word follows them.
const RUNNERS: &[&[&str]] = &[
    &["npx"],
    &["bunx"],
    &["pnpx"],
    &["bun", "x"],
    &["pnpm", "exec"],
    &["pnpm", "dlx"],
    &["yarn", "dlx"],
    &["uv", "run"],
    &["uvx"],
    &["poetry", "run"],
    &["pipx", "run"],
    &["bundle", "exec"],
    &["mise", "exec"],
    &["nix", "run"],
];

/// Phrases that start a service, at the command position.
const SERVICES: &[&[&str]] = &[
    &["npm", "start"],
    &["npm", "run", "dev"],
    &["npm", "run", "start"],
    &["npm", "run", "serve"],
    &["npm", "run", "watch"],
    &["pnpm", "dev"],
    &["pnpm", "start"],
    &["pnpm", "run", "dev"],
    &["yarn", "dev"],
    &["yarn", "start"],
    &["bun", "dev"],
    &["bun", "run", "dev"],
    &["deno", "task", "dev"],
    &["serve"],
    &["rails", "s"],
    &["rails", "server"],
    &["python", "-m", "http.server"],
    &["python3", "-m", "http.server"],
    &["uvicorn"],
    &["gunicorn"],
    &["flask", "run"],
    &["fastapi", "dev"],
    &["next", "dev"],
    &["nuxt", "dev"],
    &["astro", "dev"],
    &["hugo", "server"],
    &["jekyll", "serve"],
    &["mkdocs", "serve"],
    &["cargo", "watch"],
    &["cargo", "leptos", "watch"],
    &["trunk", "serve"],
    &["air"],
    &["nodemon"],
    &["kubectl", "port-forward"],
    &["tail", "-f"],
    &["tail", "-F"],
    &["journalctl", "-f"],
    &["php", "artisan", "serve"],
    &["mix", "phx.server"],
];

const TESTS: &[&[&str]] = &[
    &["cargo", "test"],
    &["cargo", "nextest"],
    &["go", "test"],
    &["pytest"],
    &["python", "-m", "pytest"],
    &["python3", "-m", "pytest"],
    &["npm", "test"],
    &["npm", "run", "test"],
    &["pnpm", "test"],
    &["yarn", "test"],
    &["bun", "test"],
    &["jest"],
    &["vitest"],
    &["mocha"],
    &["playwright", "test"],
    &["bats"],
    &["make", "test"],
    &["make", "check"],
    &["mix", "test"],
    &["rspec"],
    &["ctest"],
    &["tox"],
    &["nox"],
    &["deno", "test"],
    &["swift", "test"],
    &["gradle", "test"],
    &["mvn", "test"],
];

const BUILDS: &[&[&str]] = &[
    &["cargo", "build"],
    &["cargo", "check"],
    &["cargo", "clippy"],
    &["cargo", "install"],
    &["go", "build"],
    &["make"],
    &["cmake"],
    &["ninja"],
    &["npm", "run", "build"],
    &["pnpm", "build"],
    &["pnpm", "run", "build"],
    &["yarn", "build"],
    &["bun", "run", "build"],
    &["vite", "build"],
    &["tsc"],
    &["gradle"],
    &["mvn"],
    &["bazel", "build"],
    &["nix", "build"],
    &["docker", "build"],
    &["swift", "build"],
    &["zig", "build"],
    &["just"],
];

const DEPLOYS: &[&[&str]] = &[
    &["kubectl", "apply"],
    &["terraform", "apply"],
    &["tofu", "apply"],
    &["fly", "deploy"],
    &["flyctl", "deploy"],
    &["vercel"],
    &["netlify", "deploy"],
    &["helm", "upgrade"],
    &["helm", "install"],
    &["ansible-playbook"],
    &["wrangler", "deploy"],
    &["firebase", "deploy"],
    &["cdk", "deploy"],
    &["sam", "deploy"],
    &["serverless", "deploy"],
];

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether program `name` (a path, or `-zsh` as a login shell shows) is an
/// interactive shell.
pub fn is_shell(name: &str) -> bool {
    SHELLS.contains(&basename(name.trim_start_matches('-')))
}

fn starts_with(words: &[&str], phrase: &[&str]) -> bool {
    words.len() >= phrase.len() && words.iter().zip(phrase).all(|(w, p)| w == p)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(name, _)| !name.is_empty() && !name.contains('/'))
}

/// The words of one segment from its head: past env assignments and
/// wrappers (and their flags), program names as basenames.
fn head(words: &[&str]) -> Vec<String> {
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if is_assignment(w) || WRAPPERS.contains(&basename(w)) || (i > 0 && w.starts_with('-')) {
            i += 1;
        } else {
            break;
        }
    }
    let mut out: Vec<String> = words[i..].iter().map(|w| w.to_string()).collect();
    if let Some(first) = out.first_mut() {
        *first = basename(first).to_string();
    }
    out
}

/// The words from the command word on: past a package runner and its
/// flags (a `-p`/`--package` flag skips its value).
fn command_word<'a>(words: &[&'a str]) -> Option<Vec<&'a str>> {
    let runner = RUNNERS.iter().find(|r| starts_with(words, r))?;
    let mut i = runner.len();
    while i < words.len() && words[i].starts_with('-') {
        i += if matches!(words[i], "-p" | "--package") {
            2
        } else {
            1
        };
    }
    Some(words.get(i..).unwrap_or_default().to_vec())
}

fn segment_class(segment: &str) -> Option<Class> {
    // shortcut: quotes are dropped, not parsed; a quoted space splits a word.
    let raw: Vec<&str> = segment
        .split_whitespace()
        .map(|w| w.trim_matches(['\'', '"']))
        .filter(|w| !w.is_empty())
        .collect();
    let head = head(&raw);
    let head: Vec<&str> = head.iter().map(String::as_str).collect();
    let first = *head.first()?;
    if REMOTE.contains(&first) {
        return Some(Class::Remote);
    }
    if INTERACTIVE.contains(&first) {
        return Some(Class::Interactive);
    }
    // `sh -c 'script'`: the script decides.
    if is_shell(first) {
        return head
            .iter()
            .position(|w| *w == "-c" || *w == "-lc")
            .map(|i| classify(&head[i + 1..].join(" ")))
            .filter(|c| *c != Class::Other)
            .or((head.len() == 1).then_some(Class::Interactive));
    }
    let word = command_word(&head);
    let at = |phrases: &[&[&str]]| {
        phrases
            .iter()
            .any(|p| starts_with(&head, p) || word.as_deref().is_some_and(|w| starts_with(w, p)))
    };
    let watching = head
        .windows(2)
        .any(|w| matches!(w[0], "--watch" | "--watchAll") && !matches!(w[1], "false" | "0"))
        || head
            .last()
            .is_some_and(|w| matches!(*w, "--watch" | "--watchAll"))
        || head.contains(&"--watch=true");
    let vite_dev =
        word.as_deref().unwrap_or(&head).first() == Some(&"vite") && !head.contains(&"build");
    let compose_up = (starts_with(&head, &["docker", "compose", "up"])
        || starts_with(&head, &["docker-compose", "up"]))
        && !head.iter().any(|w| {
            matches!(
                *w,
                "-d" | "--detach" | "--abort-on-container-exit" | "--exit-code-from"
            )
        });
    if at(SERVICES) || vite_dev || compose_up || (watching && first != "gh" && first != "kubectl") {
        return Some(Class::Service);
    }
    if at(TESTS) {
        return Some(Class::Test);
    }
    if at(DEPLOYS) {
        return Some(Class::Deploy);
    }
    if at(BUILDS) {
        return Some(Class::Build);
    }
    Some(Class::Other)
}

/// Classify a command line. Each segment between `&`, `|`, `;`, newlines,
/// parentheses and backticks is judged from its head; a service anywhere
/// wins, then the first segment that is not `Other` (`cd x && cargo test`
/// is a test).
pub fn classify(line: &str) -> Class {
    let classes: Vec<Class> = line
        .split(['&', '|', ';', '\n', '\r', '(', ')', '`'])
        .filter_map(segment_class)
        .collect();
    let rank = |c: &Class| match c {
        Class::Service => 0,
        Class::Remote => 1,
        Class::Other => 3,
        _ => 2,
    };
    classes.into_iter().min_by_key(rank).unwrap_or(Class::Other)
}

/// Classify an argv as Zellij reports it for a pane.
pub fn classify_argv(argv: &[String]) -> Class {
    classify(&argv.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_commands_by_their_head() {
        use Class::*;
        let cases = [
            ("cargo test --workspace", Test),
            ("cd crates && cargo test", Test),
            ("RUST_LOG=1 cargo build --release", Build),
            ("npm run dev", Service),
            ("npx vite", Service),
            ("npx vite build", Build),
            ("pnpm exec vitest run", Test),
            ("uv run pytest -x", Test),
            ("tail -f log.txt", Service),
            ("jest --watch", Service),
            ("jest --watch false", Test),
            ("gh run watch --watch", Other),
            ("docker compose up", Service),
            ("docker compose up -d", Other),
            ("ssh box", Remote),
            ("nvim README.md", Interactive),
            ("/usr/bin/less x", Interactive),
            ("bash -c 'cargo test'", Test),
            ("zsh", Interactive),
            ("pytest tests/test_serve.py", Test),
            ("make dev-deps", Build),
            ("cd serve && cargo test", Test),
            ("terraform apply -auto-approve", Deploy),
            ("ls -la", Other),
            ("sudo -E make install", Build),
            ("", Other),
        ];
        for (line, want) in cases {
            assert_eq!(classify(line), want, "{line:?}");
        }
    }

    #[test]
    fn knows_shells_and_kinds() {
        assert!(is_shell("-zsh"));
        assert!(is_shell("/opt/homebrew/bin/fish"));
        assert!(!is_shell("cargo"));
        for c in [Class::Build, Class::Test, Class::Service, Class::Remote] {
            assert_eq!(Class::from_kind(c.kind()), Some(c));
        }
        assert_eq!(Class::from_kind("claude"), None);
        assert!(Class::Service.is_steady() && !Class::Test.is_steady());
    }
}
