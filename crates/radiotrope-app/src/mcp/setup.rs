//! Ready-made lines that add Radiotrope to common agents, for the Agents
//! dialog: one for this computer (`radiotrope --mcp`) and one for the
//! network server
//!
//! Most agents have an "add MCP server" command; the rest (Cursor, Claude
//! Desktop) read a JSON settings file, so they get the JSON to paste.
//! Claude Desktop's file only takes programs on this computer, so it has no
//! network line.

use serde_json::json;

/// An agent the dialog knows how to set up
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentApp {
    ClaudeCode,
    Codex,
    Gemini,
    VsCode,
    /// `~/.cursor/mcp.json`
    Cursor,
    /// `claude_desktop_config.json`: programs on this computer only
    ClaudeDesktop,
}

impl AgentApp {
    /// In the order the dialog lists them
    pub const ALL: [AgentApp; 6] = [
        AgentApp::ClaudeCode,
        AgentApp::ClaudeDesktop,
        AgentApp::Codex,
        AgentApp::Gemini,
        AgentApp::VsCode,
        AgentApp::Cursor,
    ];

    /// Saved in the settings
    pub fn id(self) -> &'static str {
        match self {
            AgentApp::ClaudeCode => "claude-code",
            AgentApp::Codex => "codex",
            AgentApp::Gemini => "gemini",
            AgentApp::VsCode => "vscode",
            AgentApp::Cursor => "cursor",
            AgentApp::ClaudeDesktop => "claude-desktop",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentApp::ClaudeCode => "Claude Code",
            AgentApp::Codex => "Codex CLI",
            AgentApp::Gemini => "Gemini CLI",
            AgentApp::VsCode => "VS Code",
            AgentApp::Cursor => "Cursor",
            AgentApp::ClaudeDesktop => "Claude Desktop",
        }
    }

    /// Claude Code for an unknown or missing id
    pub fn from_id(id: Option<&str>) -> AgentApp {
        // Cursor and Claude Desktop shared one "json" entry at first
        if id == Some("json") {
            return AgentApp::Cursor;
        }
        Self::ALL
            .into_iter()
            .find(|app| Some(app.id()) == id)
            .unwrap_or(AgentApp::ClaudeCode)
    }

    /// Help shown next to the line for this computer, if any
    pub fn local_note(self) -> &'static str {
        self.local_note_for(SHELL)
    }

    fn local_note_for(self, shell: Shell) -> &'static str {
        match self {
            AgentApp::Cursor => "Add to ~/.cursor/mcp.json (or .cursor/mcp.json in a project).",
            AgentApp::ClaudeDesktop => {
                "Add to claude_desktop_config.json (Settings > Developer > Edit Config), \
                 then restart Claude Desktop."
            }
            AgentApp::VsCode if shell == Shell::Windows => COMMAND_PROMPT,
            _ => "",
        }
    }

    /// Help shown next to the network line, if any; `token` says whether
    /// the line carries one
    pub fn network_note(self, token: bool) -> &'static str {
        self.network_note_for(token, SHELL)
    }

    fn network_note_for(self, token: bool, shell: Shell) -> &'static str {
        match self {
            AgentApp::Codex if token => {
                "Codex reads the token from the RADIOTROPE_TOKEN environment variable: \
                 set it to the token above."
            }
            AgentApp::Cursor => "Add to ~/.cursor/mcp.json (or .cursor/mcp.json in a project).",
            AgentApp::VsCode if shell == Shell::Windows => COMMAND_PROMPT,
            _ => "",
        }
    }
}

/// The note for a Windows line that only Command Prompt reads right
const COMMAND_PROMPT: &str =
    "Run it in Command Prompt: PowerShell reads the quotes inside it differently.";

/// How the line is quoted: for a Unix shell (sh, bash, zsh), or for
/// Command Prompt on Windows. PowerShell reads Command Prompt's double
/// quotes the same way, except for the escaped ones inside VS Code's JSON,
/// so that line says which one to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shell {
    Unix,
    Windows,
}

const SHELL: Shell = if cfg!(windows) {
    Shell::Windows
} else {
    Shell::Unix
};

/// This program's path, which local agents run with `--mcp`
pub fn this_program() -> String {
    // An AppImage runs from a temporary mount; the file itself stays put
    std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "radiotrope".into())
}

/// The line that adds Radiotrope on this computer
pub fn local_line(app: AgentApp, exe: &str) -> String {
    local_line_for(app, exe, SHELL)
}

/// The line that adds Radiotrope over the network; `token` is `None` when
/// the player wants none, and may be empty until one is made. `None` for an
/// agent that can't use the network.
pub fn network_line(app: AgentApp, url: &str, token: Option<&str>) -> Option<String> {
    network_line_for(app, url, token, SHELL)
}

/// Shown in place of the network line when there is none
pub fn no_network_line(app: AgentApp) -> &'static str {
    match app {
        AgentApp::ClaudeDesktop => "Claude Desktop only takes agents on this computer",
        _ => "Ready once there is a token",
    }
}

fn local_line_for(app: AgentApp, exe: &str, shell: Shell) -> String {
    let path = quote(exe, shell);
    match app {
        AgentApp::ClaudeCode => format!("claude mcp add radiotrope -- {path} --mcp"),
        AgentApp::Codex => format!("codex mcp add radiotrope -- {path} --mcp"),
        // "--" keeps --mcp from being read as Gemini's own option
        AgentApp::Gemini => format!("gemini mcp add -s user radiotrope {path} -- --mcp"),
        AgentApp::VsCode => vscode_line(
            json!({ "name": "radiotrope", "command": exe, "args": ["--mcp"] }),
            shell,
        ),
        AgentApp::Cursor | AgentApp::ClaudeDesktop => json!({
            "mcpServers": { "radiotrope": { "command": exe, "args": ["--mcp"] } }
        })
        .to_string(),
    }
}

fn network_line_for(app: AgentApp, url: &str, token: Option<&str>, shell: Shell) -> Option<String> {
    let bearer = token.map(|t| format!("Bearer {}", if t.is_empty() { "<token>" } else { t }));
    // The JSON lines take it as it is
    let json_url = url;
    let url = quote(url, shell);
    Some(match app {
        AgentApp::ClaudeCode => {
            let mut line = format!("claude mcp add --transport http radiotrope {url}");
            if let Some(bearer) = &bearer {
                line += &format!(" --header \"Authorization: {bearer}\"");
            }
            line
        }
        // Codex takes a token only from an environment variable
        AgentApp::Codex => {
            let mut line = format!("codex mcp add radiotrope --url {url}");
            if bearer.is_some() {
                line += " --bearer-token-env-var RADIOTROPE_TOKEN";
            }
            line
        }
        AgentApp::Gemini => {
            let mut line = "gemini mcp add -s user --transport http".to_string();
            if let Some(bearer) = &bearer {
                line += &format!(" --header \"Authorization: {bearer}\"");
            }
            line + &format!(" radiotrope {url}")
        }
        AgentApp::VsCode => {
            let mut server = json!({ "name": "radiotrope", "type": "http", "url": json_url });
            if let Some(bearer) = &bearer {
                server["headers"] = json!({ "Authorization": bearer });
            }
            vscode_line(server, shell)
        }
        AgentApp::Cursor => {
            let mut server = json!({ "url": json_url });
            if let Some(bearer) = &bearer {
                server["headers"] = json!({ "Authorization": bearer });
            }
            json!({ "mcpServers": { "radiotrope": server } }).to_string()
        }
        // Its settings file takes programs only; remote servers are added
        // as connectors, which connect from Anthropic's cloud
        AgentApp::ClaudeDesktop => return None,
    })
}

/// `code --add-mcp` with the server as one JSON argument
fn vscode_line(server: serde_json::Value, shell: Shell) -> String {
    let server = server.to_string();
    match shell {
        Shell::Unix => format!("code --add-mcp {}", quote(&server, shell)),
        // As in VS Code's own docs: double quotes, inner ones escaped. Only
        // Command Prompt reads these as meant (see COMMAND_PROMPT).
        Shell::Windows => format!("code --add-mcp \"{}\"", server.replace('"', "\\\"")),
    }
}

/// One word for the shell: as it is when nothing in it is special there,
/// otherwise quoted. A Unix shell gets single quotes, inside which only a
/// single quote needs care (`'\''`: end, an escaped quote, start again), so
/// `$`, backticks and spaces stay as they are. Command Prompt gets double
/// quotes, which a Windows path never contains.
fn quote(word: &str, shell: Shell) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-:".contains(c);
    match shell {
        Shell::Unix
            if !word.is_empty() && word.chars().all(|c| plain(c) || "+,=@%".contains(c)) =>
        {
            word.to_string()
        }
        Shell::Unix => format!("'{}'", word.replace('\'', r"'\''")),
        Shell::Windows if !word.is_empty() && word.chars().all(|c| plain(c) || c == '\\') => {
            word.to_string()
        }
        Shell::Windows => format!("\"{word}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://192.168.1.20:8765/mcp";

    #[test]
    fn every_agent_has_both_lines() {
        for app in AgentApp::ALL {
            assert_eq!(AgentApp::from_id(Some(app.id())), app);
            let local = local_line_for(app, "/usr/bin/radiotrope", Shell::Unix);
            assert!(local.contains("/usr/bin/radiotrope"), "{local}");
            assert!(local.contains("--mcp"), "{local}");
            match network_line_for(app, URL, Some("abc"), Shell::Unix) {
                Some(network) => assert!(network.contains(URL), "{network}"),
                None => assert_eq!(app, AgentApp::ClaudeDesktop),
            }
        }
        assert_eq!(AgentApp::from_id(Some("json")), AgentApp::Cursor);
        assert_eq!(AgentApp::from_id(Some("nope")), AgentApp::ClaudeCode);
        assert_eq!(AgentApp::from_id(None), AgentApp::ClaudeCode);
    }

    #[test]
    fn claude_code_lines() {
        assert_eq!(
            local_line_for(AgentApp::ClaudeCode, "/usr/bin/radiotrope", Shell::Unix),
            "claude mcp add radiotrope -- /usr/bin/radiotrope --mcp"
        );
        assert_eq!(
            local_line_for(
                AgentApp::ClaudeCode,
                r"C:\Program Files\Radiotrope\radiotrope.exe",
                Shell::Windows
            ),
            r#"claude mcp add radiotrope -- "C:\Program Files\Radiotrope\radiotrope.exe" --mcp"#
        );
        assert_eq!(
            network_line_for(AgentApp::ClaudeCode, URL, Some(""), Shell::Unix).unwrap(),
            format!(
                "claude mcp add --transport http radiotrope {URL} --header \"Authorization: Bearer <token>\""
            )
        );
    }

    #[test]
    fn the_json_lines_are_valid_json() {
        let exe = r"C:\Program Files\Radiotrope\radiotrope.exe";
        let local: serde_json::Value = serde_json::from_str(&local_line_for(
            AgentApp::ClaudeDesktop,
            exe,
            Shell::Windows,
        ))
        .unwrap();
        assert_eq!(local["mcpServers"]["radiotrope"]["command"], exe);
        let network: serde_json::Value = serde_json::from_str(
            &network_line_for(AgentApp::Cursor, URL, Some("abc"), Shell::Unix).unwrap(),
        )
        .unwrap();
        assert_eq!(
            network["mcpServers"]["radiotrope"]["headers"]["Authorization"],
            "Bearer abc"
        );
    }

    #[test]
    fn without_a_token_the_lines_carry_none() {
        for app in AgentApp::ALL {
            if let Some(line) = network_line_for(app, URL, None, Shell::Unix) {
                assert!(line.contains(URL), "{line}");
                assert!(!line.contains("Bearer"), "{line}");
                assert!(!line.contains("TOKEN"), "{line}");
            }
        }
        assert_eq!(AgentApp::Codex.network_note(false), "");
        assert_eq!(
            network_line_for(AgentApp::ClaudeCode, URL, None, Shell::Unix).unwrap(),
            format!("claude mcp add --transport http radiotrope {URL}")
        );
        assert_eq!(
            network_line_for(AgentApp::Gemini, URL, None, Shell::Unix).unwrap(),
            format!("gemini mcp add -s user --transport http radiotrope {URL}")
        );
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        let line = |exe: &str, shell| local_line_for(AgentApp::ClaudeCode, exe, shell);
        assert_eq!(
            line("/opt/My Apps/radiotrope", Shell::Unix),
            "claude mcp add radiotrope -- '/opt/My Apps/radiotrope' --mcp"
        );
        // Neither $ nor a backtick is read by the shell inside single quotes
        assert_eq!(
            line("/home/a$b/`x`/radiotrope", Shell::Unix),
            "claude mcp add radiotrope -- '/home/a$b/`x`/radiotrope' --mcp"
        );
        // A single quote ends the quoting, comes escaped, and it goes on
        assert_eq!(
            line("/home/o'neil/radiotrope", Shell::Unix),
            r"claude mcp add radiotrope -- '/home/o'\''neil/radiotrope' --mcp"
        );
        assert_eq!(
            line(r"C:\Tools&More\radiotrope.exe", Shell::Windows),
            r#"claude mcp add radiotrope -- "C:\Tools&More\radiotrope.exe" --mcp"#
        );
        assert_eq!(
            line(r"C:\radiotrope\radiotrope.exe", Shell::Windows),
            r"claude mcp add radiotrope -- C:\radiotrope\radiotrope.exe --mcp"
        );
        // VS Code's JSON keeps working with a quote in the path
        let vscode = local_line_for(AgentApp::VsCode, "/home/o'neil/rt", Shell::Unix);
        assert_eq!(
            vscode,
            r#"code --add-mcp '{"args":["--mcp"],"command":"/home/o'\''neil/rt","name":"radiotrope"}'"#
        );
    }

    #[test]
    fn only_the_windows_vs_code_lines_name_their_shell() {
        for app in AgentApp::ALL {
            let windows = app == AgentApp::VsCode;
            assert_eq!(
                app.local_note_for(Shell::Windows) == COMMAND_PROMPT,
                windows
            );
            assert_eq!(
                app.network_note_for(false, Shell::Windows) == COMMAND_PROMPT,
                windows
            );
            assert_ne!(app.local_note_for(Shell::Unix), COMMAND_PROMPT);
            assert_ne!(app.network_note_for(true, Shell::Unix), COMMAND_PROMPT);
        }
    }

    #[test]
    fn vs_code_gets_its_json_quoted_for_the_shell() {
        let unix = local_line_for(AgentApp::VsCode, "/usr/bin/radiotrope", Shell::Unix);
        assert_eq!(
            unix,
            r#"code --add-mcp '{"args":["--mcp"],"command":"/usr/bin/radiotrope","name":"radiotrope"}'"#
        );
        let windows = local_line_for(AgentApp::VsCode, r"C:\radiotrope.exe", Shell::Windows);
        assert_eq!(
            windows,
            r#"code --add-mcp "{\"args\":[\"--mcp\"],\"command\":\"C:\\radiotrope.exe\",\"name\":\"radiotrope\"}""#
        );
    }
}
