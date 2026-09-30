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
        match self {
            AgentApp::Cursor => "Add to ~/.cursor/mcp.json (or .cursor/mcp.json in a project).",
            AgentApp::ClaudeDesktop => {
                "Add to claude_desktop_config.json (Settings > Developer > Edit Config), \
                 then restart Claude Desktop."
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
            AgentApp::Cursor => "Add to ~/.cursor/mcp.json (or .cursor/mcp.json in a project).",
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
/// until one is made. `None` for an agent that can't use the network.
pub fn network_line(app: AgentApp, url: &str, token: &str) -> Option<String> {
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
        AgentApp::Cursor | AgentApp::ClaudeDesktop => json!({
            "mcpServers": { "radiotrope": { "command": exe, "args": ["--mcp"] } }
        })
        .to_string(),
    }
}

fn network_line_for(app: AgentApp, url: &str, token: &str, shell: Shell) -> Option<String> {
    let token = if token.is_empty() { "<token>" } else { token };
    let bearer = format!("Bearer {token}");
    Some(match app {
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
        AgentApp::Cursor => json!({
            "mcpServers": {
                "radiotrope": { "url": url, "headers": { "Authorization": bearer } }
            }
        })
        .to_string(),
        // Its settings file takes programs only; remote servers are added
        // as connectors, which connect from Anthropic's cloud
        AgentApp::ClaudeDesktop => return None,
    })
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
            match network_line_for(app, URL, "abc", Shell::Unix) {
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
            network_line_for(AgentApp::ClaudeCode, URL, "", Shell::Unix).unwrap(),
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
            &network_line_for(AgentApp::Cursor, URL, "abc", Shell::Unix).unwrap(),
        )
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
