//! Ready-made lines that add Radiotrope to common agents, for the Agents
//! dialog: one for this computer (`radiotrope --mcp`) and one for the
//! network server
//!
//! Most agents have an "add MCP server" command; the rest (Cursor, Claude
//! Desktop) read a JSON settings file, so they get the JSON to paste.

use serde_json::json;

/// An agent the dialog knows how to set up
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentApp {
    ClaudeCode,
    Codex,
    Gemini,
    VsCode,
    /// Cursor, Claude Desktop and others with an `mcpServers` JSON file
    Json,
}

impl AgentApp {
    /// In the order the dialog lists them
    pub const ALL: [AgentApp; 5] = [
        AgentApp::ClaudeCode,
        AgentApp::Codex,
        AgentApp::Gemini,
        AgentApp::VsCode,
        AgentApp::Json,
    ];

    /// Saved in the settings
    pub fn id(self) -> &'static str {
        match self {
            AgentApp::ClaudeCode => "claude-code",
            AgentApp::Codex => "codex",
            AgentApp::Gemini => "gemini",
            AgentApp::VsCode => "vscode",
            AgentApp::Json => "json",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentApp::ClaudeCode => "Claude Code",
            AgentApp::Codex => "Codex CLI",
            AgentApp::Gemini => "Gemini CLI",
            AgentApp::VsCode => "VS Code",
            AgentApp::Json => "Cursor, Claude Desktop (JSON)",
        }
    }

    /// Claude Code for an unknown or missing id
    pub fn from_id(id: Option<&str>) -> AgentApp {
        Self::ALL
            .into_iter()
            .find(|app| Some(app.id()) == id)
            .unwrap_or(AgentApp::ClaudeCode)
    }

    /// Help shown next to the line for this computer, if any
    pub fn local_note(self) -> &'static str {
        match self {
            AgentApp::Json => {
                "Paste into the agent's settings: claude_desktop_config.json for Claude \
                 Desktop, ~/.cursor/mcp.json for Cursor."
            }
            _ => "",
        }
    }

    /// Help shown next to the network line, if any
    pub fn network_note(self) -> &'static str {
        match self {
            AgentApp::Codex => {
                "Codex reads the token from the RADIOTROPE_TOKEN environment variable: \
                 set it to the token above."
            }
            AgentApp::Json => {
                "For Cursor. Claude Desktop only takes agents on this computer (the line \
                 above)."
            }
            _ => "",
        }
    }
}

/// How the line is quoted: for a Unix shell, or for Windows (cmd and
/// PowerShell both take double quotes)
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

/// The line that adds Radiotrope over the network; `token` may be empty
/// until one is made
pub fn network_line(app: AgentApp, url: &str, token: &str) -> String {
    network_line_for(app, url, token, SHELL)
}

fn local_line_for(app: AgentApp, exe: &str, shell: Shell) -> String {
    let path = quote_path(exe);
    match app {
        AgentApp::ClaudeCode => format!("claude mcp add radiotrope -- {path} --mcp"),
        AgentApp::Codex => format!("codex mcp add radiotrope -- {path} --mcp"),
        // "--" keeps --mcp from being read as Gemini's own option
        AgentApp::Gemini => format!("gemini mcp add -s user radiotrope {path} -- --mcp"),
        AgentApp::VsCode => vscode_line(
            json!({ "name": "radiotrope", "command": exe, "args": ["--mcp"] }),
            shell,
        ),
        AgentApp::Json => json!({
            "mcpServers": { "radiotrope": { "command": exe, "args": ["--mcp"] } }
        })
        .to_string(),
    }
}

fn network_line_for(app: AgentApp, url: &str, token: &str, shell: Shell) -> String {
    let token = if token.is_empty() { "<token>" } else { token };
    let bearer = format!("Bearer {token}");
    match app {
        AgentApp::ClaudeCode => format!(
            "claude mcp add --transport http radiotrope {url} --header \"Authorization: {bearer}\""
        ),
        // Codex takes a token only from an environment variable
        AgentApp::Codex => {
            format!("codex mcp add radiotrope --url {url} --bearer-token-env-var RADIOTROPE_TOKEN")
        }
        AgentApp::Gemini => format!(
            "gemini mcp add -s user --transport http --header \"Authorization: {bearer}\" radiotrope {url}"
        ),
        AgentApp::VsCode => vscode_line(
            json!({
                "name": "radiotrope",
                "type": "http",
                "url": url,
                "headers": { "Authorization": bearer },
            }),
            shell,
        ),
        AgentApp::Json => json!({
            "mcpServers": {
                "radiotrope": { "url": url, "headers": { "Authorization": bearer } }
            }
        })
        .to_string(),
    }
}

/// `code --add-mcp` with the server as one JSON argument
fn vscode_line(server: serde_json::Value, shell: Shell) -> String {
    let server = server.to_string();
    match shell {
        // JSON has no single quotes, so they wrap it whole
        Shell::Unix => format!("code --add-mcp '{server}'"),
        // As in VS Code's own docs: double quotes, inner ones escaped
        Shell::Windows => format!("code --add-mcp \"{}\"", server.replace('"', "\\\"")),
    }
}

fn quote_path(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_string()
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
            let network = network_line_for(app, URL, "abc", Shell::Unix);
            assert!(network.contains(URL), "{network}");
        }
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
            network_line_for(AgentApp::ClaudeCode, URL, "", Shell::Unix),
            format!(
                "claude mcp add --transport http radiotrope {URL} --header \"Authorization: Bearer <token>\""
            )
        );
    }

    #[test]
    fn the_json_lines_are_valid_json() {
        let exe = r"C:\Program Files\Radiotrope\radiotrope.exe";
        let local: serde_json::Value =
            serde_json::from_str(&local_line_for(AgentApp::Json, exe, Shell::Windows)).unwrap();
        assert_eq!(local["mcpServers"]["radiotrope"]["command"], exe);
        let network: serde_json::Value =
            serde_json::from_str(&network_line_for(AgentApp::Json, URL, "abc", Shell::Unix))
                .unwrap();
        assert_eq!(
            network["mcpServers"]["radiotrope"]["headers"]["Authorization"],
            "Bearer abc"
        );
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
