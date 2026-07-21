//! Pure, unit-testable helpers for spawning Claude Code as the kontur agent.
//!
//! Nothing in this module performs I/O or spawns a process; those concerns live
//! in the bin. The three public functions are each independently tested.

/// The program and arguments required to launch Claude Code as the agent.
#[derive(Debug, Clone)]
pub struct ClaudeCmd {
    pub program: String,
    pub args: Vec<String>,
}

/// Compose the spawn command for Claude Code.
///
/// Enforcement is permission-level: native mutation tools (Write, Edit,
/// MultiEdit, NotebookEdit, Bash) are denied via CC's own flag system; only
/// `mcp__kontur__*` tools are allowed.
///
/// Honest caveat: this relies on Claude Code's `--disallowedTools` /
/// `--allowedTools` / `--permission-mode` flags, not an OS-level sandbox.
pub fn build_claude_command(mcp_config_path: &str, prompt: &str) -> ClaudeCmd {
    ClaudeCmd {
        program: "claude".into(),
        args: vec![
            "-p".into(),
            prompt.to_string(),
            "--mcp-config".into(),
            mcp_config_path.to_string(),
            "--allowedTools".into(),
            "mcp__kontur__*".into(),
            "--disallowedTools".into(),
            "Write".into(),
            "Edit".into(),
            "MultiEdit".into(),
            "NotebookEdit".into(),
            "Bash".into(),
            "--permission-mode".into(),
            "default".into(),
        ],
    }
}

/// Return the JSON content for the MCP config file that bridges Claude Code's
/// stdio to the agent port. `bridge_program` is the command Claude spawns —
/// the kontur executable itself, invoked as `kontur mcp-bridge <port>` — so
/// there is no external `nc` dependency.
///
/// The resulting string is valid JSON:
/// `{"mcpServers":{"kontur":{"command":"<bridge>","args":["mcp-bridge","<port>"]}}}`.
pub fn mcp_config_json(bridge_program: &str, agent_port: u16) -> String {
    // JSON-escape the program path (Windows paths contain backslashes).
    let prog = bridge_program.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"{{"mcpServers":{{"kontur":{{"command":"{prog}","args":["mcp-bridge","{agent_port}"]}}}}}}"#
    )
}

/// Build the protocol prompt handed to `claude -p`.
///
/// Embeds `session_prompt` under "Instruction:" and prepends terse protocol
/// instructions that describe how the agent must use kontur MCP tools.
pub fn agent_prompt(session_prompt: &str) -> String {
    format!(
        "You are the kontur coding agent. \
Use ONLY the kontur MCP tools — never use native file or shell tools. \
FIRST call `propose_plan` with a task list of bounded, single-concern tasks and \
wait for approval before writing any code. \
If propose_plan returns an error containing a steer, revise your task list per \
the steer and call propose_plan again with the revised list. \
The propose_plan response (on success) contains the APPROVED task list, which \
the operators may have edited, deleted from, or reordered — execute exactly that \
list, in that order, using its task numbering. \
Then, for each task: write files with `write_file`, verify with `run_command` if \
needed, then call `propose_task_complete` with the task_id and WAIT for the \
verdict. \
If the verdict is a rejection, the error carries a steer — apply it and \
re-propose the SAME task_id. \
Finish after the last approved task. \
Do not take any action outside this protocol. \
\nInstruction: {session_prompt}"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_claude_command_contains_deny_allow_flags() {
        let cmd = build_claude_command("/tmp/mcp.json", "do the thing");
        assert_eq!(cmd.program, "claude");

        let args = &cmd.args;

        // -p flag and its value
        let p_pos = args
            .iter()
            .position(|a| a == "-p")
            .expect("-p flag missing");
        assert_eq!(args[p_pos + 1], "do the thing");

        // --mcp-config
        let cfg_pos = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config missing");
        assert_eq!(args[cfg_pos + 1], "/tmp/mcp.json");

        // --allowedTools with the kontur glob
        let allow_pos = args
            .iter()
            .position(|a| a == "--allowedTools")
            .expect("--allowedTools missing");
        assert_eq!(args[allow_pos + 1], "mcp__kontur__*");

        // --disallowedTools followed by all five denied tools
        let deny_pos = args
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("--disallowedTools missing");
        let denied: Vec<&str> = args[deny_pos + 1..]
            .iter()
            .take_while(|a| !a.starts_with("--"))
            .map(String::as_str)
            .collect();
        for tool in &["Write", "Edit", "MultiEdit", "NotebookEdit", "Bash"] {
            assert!(
                denied.contains(tool),
                "--disallowedTools is missing '{tool}'"
            );
        }

        // --permission-mode default
        let pm_pos = args
            .iter()
            .position(|a| a == "--permission-mode")
            .expect("--permission-mode missing");
        assert_eq!(args[pm_pos + 1], "default");
    }

    #[test]
    fn mcp_config_json_is_valid_and_contains_port() {
        let json = mcp_config_json("/usr/local/bin/kontur", 7778);

        // Must be valid JSON.
        let v: serde_json::Value =
            serde_json::from_str(&json).expect("mcp_config_json is not valid JSON");

        // Must invoke the kontur bridge, not nc.
        assert_eq!(
            v["mcpServers"]["kontur"]["command"].as_str(),
            Some("/usr/local/bin/kontur")
        );
        let args_arr = v["mcpServers"]["kontur"]["args"]
            .as_array()
            .expect("args should be an array");
        let args: Vec<&str> = args_arr.iter().filter_map(|a| a.as_str()).collect();
        assert_eq!(args, vec!["mcp-bridge", "7778"]);
    }

    #[test]
    fn mcp_config_json_escapes_windows_paths() {
        let json = mcp_config_json(r"C:\Program Files\kontur.exe", 9999);
        // Still valid JSON despite backslashes in the path.
        let v: serde_json::Value =
            serde_json::from_str(&json).expect("windows path must produce valid JSON");
        assert_eq!(
            v["mcpServers"]["kontur"]["command"].as_str(),
            Some(r"C:\Program Files\kontur.exe")
        );
        let args = v["mcpServers"]["kontur"]["args"].as_array().unwrap();
        assert!(args.iter().any(|a| a.as_str() == Some("9999")));
    }

    #[test]
    fn agent_prompt_contains_session_prompt_and_propose_plan() {
        let prompt = agent_prompt("add auth module");
        // Must embed the session prompt.
        assert!(
            prompt.contains("add auth module"),
            "session prompt not embedded: {prompt}"
        );
        // Must mention propose_plan.
        assert!(
            prompt.contains("propose_plan"),
            "propose_plan not mentioned: {prompt}"
        );
        // Must mention the kontur tool constraint.
        assert!(
            prompt.contains("kontur MCP tools"),
            "kontur MCP tools not mentioned: {prompt}"
        );
    }
}
