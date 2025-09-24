use std::io::{
    BufReader,
    Cursor,
    Write,
    stdout,
};
use std::sync::{Arc, Mutex};

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen,
    LeaveAlternateScreen,
};
use eyre::{
    Result,
    eyre,
};
use rustyline::{
    Cmd,
    ConditionalEventHandler,
    EventContext,
    RepeatCount,
};
use skim::prelude::*;
use tempfile::NamedTempFile;

use super::context::ContextManager;
use super::util::truncate_safe_in_place;
use crate::os::Os;

// Column width constants for prompt selector skim display
const PROMPT_SELECTOR_SERVER_WIDTH: usize = 15;
const PROMPT_SELECTOR_PROMPT_WIDTH: usize = 15;
const PROMPT_SELECTOR_DESC_WIDTH: usize = 25;

// Truncation suffix for display text that exceeds column width
const TRUNCATED_SUFFIX: &str = "...";

pub struct SkimCommandSelector {
    os: Os,
    context_manager: Arc<ContextManager>,
    tool_names: Vec<String>,
}

impl SkimCommandSelector {
    /// This allows the ConditionalEventHandler handle function to be bound to a KeyEvent.
    pub fn new(os: Os, context_manager: Arc<ContextManager>, tool_names: Vec<String>) -> Self {
        Self {
            os,
            context_manager,
            tool_names,
        }
    }
}

impl ConditionalEventHandler for SkimCommandSelector {
    fn handle(&self, _evt: &rustyline::Event, _n: RepeatCount, _positive: bool, _os: &EventContext<'_>) -> Option<Cmd> {
        // Launch skim command selector with the context manager if available
        match select_command(&self.os, self.context_manager.as_ref(), &self.tool_names) {
            Ok(Some(command)) => Some(Cmd::Insert(1, command)),
            _ => {
                // If cancelled or error, do nothing
                Some(Cmd::Noop)
            },
        }
    }
}

pub struct PromptSelector {
    sender: super::prompt::PromptQuerySender,
    receiver: Arc<Mutex<super::prompt::PromptQueryResponseReceiver>>,
}

impl PromptSelector {
    pub fn new(sender: super::prompt::PromptQuerySender, receiver: super::prompt::PromptQueryResponseReceiver) -> Self {
        Self { 
            sender,
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }
}

impl ConditionalEventHandler for PromptSelector {
    fn handle(&self, _evt: &rustyline::Event, _n: RepeatCount, _positive: bool, _ctx: &EventContext<'_>) -> Option<Cmd> {
        match select_prompt_with_skim(&self.sender, &self.receiver) {
            Ok(Some(prompt)) => Some(Cmd::Insert(1, prompt)),
            _ => Some(Cmd::Noop), // Error or cancelled
        }
    }
}

pub fn get_available_commands() -> Vec<String> {
    // Import the COMMANDS array directly from prompt.rs
    // This is the single source of truth for available commands
    let commands_array = super::prompt::COMMANDS;

    let mut commands = Vec::new();
    for &cmd in commands_array {
        commands.push(cmd.to_string());
    }

    commands
}

/// Get available prompts with server grouping and sorting
pub fn get_available_prompts(
    sender: &super::prompt::PromptQuerySender, 
    receiver: &Arc<Mutex<super::prompt::PromptQueryResponseReceiver>>
) -> Result<Vec<(String, String, String, String)>> {
    let prompts = fetch_prompts_from_servers(sender, receiver)?;
    if prompts.is_empty() {
        return Ok(Vec::new());
    }
    
    let grouped_prompts = group_and_sort_prompts(prompts);
    Ok(format_prompts_for_display(grouped_prompts))
}

/// Fetch prompts using existing query mechanism
fn fetch_prompts_from_servers(
    sender: &super::prompt::PromptQuerySender,
    receiver: &Arc<Mutex<super::prompt::PromptQueryResponseReceiver>>
) -> Result<std::collections::HashMap<String, Vec<super::tool_manager::PromptBundle>>> {
    use super::tool_manager::{PromptQuery, PromptQueryResult};
    
    // Send query using same pattern as Tab completion
    sender.send(PromptQuery::List).map_err(|e| eyre!("Failed to send query: {}", e))?;
    
    let mut new_receiver = {
        let receiver_guard = receiver.lock().map_err(|e| eyre!("Failed to lock receiver: {}", e))?;
        receiver_guard.resubscribe()
    };
    
    // Use shared polling logic from prompt.rs
    let query_res = super::prompt::PromptCompleter::poll_for_query_result(&mut new_receiver)
        .map_err(|e| eyre!("Polling failed: {}", e))?;
    
    match query_res {
        PromptQueryResult::List(list) => Ok(list),
        PromptQueryResult::Search(_) => Err(eyre!("Wrong query response type received")),
    }
}

/// Group prompts by server and sort both servers and prompts alphabetically
fn group_and_sort_prompts(
    prompts: std::collections::HashMap<String, Vec<super::tool_manager::PromptBundle>>
) -> Vec<(String, Vec<(String, super::tool_manager::PromptBundle)>)> {
    // Group by server
    let mut servers_with_prompts: std::collections::HashMap<String, Vec<(String, super::tool_manager::PromptBundle)>> = std::collections::HashMap::new();
    
    for (prompt_name, bundles) in prompts {
        for bundle in bundles {
            servers_with_prompts
                .entry(bundle.server_name.clone())
                .or_insert_with(Vec::new)
                .push((prompt_name.clone(), bundle));
        }
    }
    
    // Convert to Vec and sort servers alphabetically
    let mut servers_with_prompts: Vec<_> = servers_with_prompts.into_iter().collect();
    servers_with_prompts.sort_by(|a, b| a.0.cmp(&b.0));
    
    // Sort prompts alphabetically within each server
    for (_, prompts_in_server) in &mut servers_with_prompts {
        prompts_in_server.sort_by(|a, b| a.0.cmp(&b.0));
    }
    
    servers_with_prompts
}

/// Format grouped prompts for display: (prompt_name, server, description, args)
fn format_prompts_for_display(
    servers_with_prompts: Vec<(String, Vec<(String, super::tool_manager::PromptBundle)>)>
) -> Vec<(String, String, String, String)> {
    servers_with_prompts
        .iter()
        .flat_map(|(_, prompts_in_server)| {
            prompts_in_server.iter().map(|(prompt_name, bundle)| {
                let description = bundle.prompt_get.description.as_deref().unwrap_or("No description");
                let args = bundle.format_arguments_for_display();
                
                (
                    format!("@{}", prompt_name),
                    bundle.server_name.clone(),
                    description.to_string(),
                    args
                )
            })
        })
        .collect()
}

/// Format commands for skim display
/// Create a standard set of skim options with consistent styling
fn create_skim_options(prompt: &str, multi: bool) -> Result<SkimOptions> {
    SkimOptionsBuilder::default()
        .height("100%".to_string())
        .prompt(prompt.to_string())
        .reverse(true)
        .multi(multi)
        .build()
        .map_err(|e| eyre!("Failed to build skim options: {}", e))
}

/// Run skim with the given options and items in an alternate screen
/// This helper function handles entering/exiting the alternate screen and running skim
fn run_skim_with_options(options: &SkimOptions, items: SkimItemReceiver) -> Result<Option<Vec<Arc<dyn SkimItem>>>> {
    // Enter alternate screen to prevent skim output from persisting in terminal history
    execute!(stdout(), EnterAlternateScreen).map_err(|e| eyre!("Failed to enter alternate screen: {}", e))?;

    let selected_items =
        Skim::run_with(options, Some(items)).and_then(|out| if out.is_abort { None } else { Some(out.selected_items) });

    execute!(stdout(), LeaveAlternateScreen).map_err(|e| eyre!("Failed to leave alternate screen: {}", e))?;

    Ok(selected_items)
}

/// Extract string selections from skim items
fn extract_selections(items: Vec<Arc<dyn SkimItem>>) -> Vec<String> {
    items.iter().map(|item| item.output().to_string()).collect()
}

/// Launch skim with the given items and return the selected item
pub fn launch_skim_selector(items: &[String], prompt: &str, multi: bool) -> Result<Option<Vec<String>>> {
    let mut temp_file_for_skim_input = NamedTempFile::new()?;
    temp_file_for_skim_input.write_all(items.join("\n").as_bytes())?;

    let options = create_skim_options(prompt, multi)?;
    let item_reader = SkimItemReader::default();
    let items = item_reader.of_bufread(BufReader::new(std::fs::File::open(temp_file_for_skim_input.path())?));

    // Run skim and get selected items
    match run_skim_with_options(&options, items)? {
        Some(items) if !items.is_empty() => {
            let selections = extract_selections(items);
            Ok(Some(selections))
        },
        _ => Ok(None), // User cancelled or no selection
    }
}

/// Select files using skim
pub fn select_files_with_skim() -> Result<Option<Vec<String>>> {
    // Create skim options with appropriate settings
    let options = create_skim_options("Select files: ", true)?;

    // Create a command that will be executed by skim
    // This command checks if git is installed and if we're in a git repo
    // Otherwise falls back to find command
    let find_cmd = r#"
    # Check if git is available and we're in a git repo
    if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree &>/dev/null; then
        # Git repository - respect .gitignore
        { git ls-files; git ls-files --others --exclude-standard; } | sort | uniq
    else
        # Not a git repository or git not installed - use find command
        find . -type f -not -path '*/\.*'
    fi
    "#;

    // Create a command collector that will execute the find command
    let item_reader = SkimItemReader::default();
    let items = item_reader.of_bufread(BufReader::new(
        std::process::Command::new("sh")
            .args(["-c", find_cmd])
            .stdout(std::process::Stdio::piped())
            .spawn()?
            .stdout
            .ok_or_else(|| eyre!("Failed to get stdout from command"))?,
    ));

    // Run skim with the command output as a stream
    match run_skim_with_options(&options, items)? {
        Some(items) if !items.is_empty() => {
            let selections = extract_selections(items);
            Ok(Some(selections))
        },
        _ => Ok(None), // User cancelled or no selection
    }
}

/// Select context paths using skim
pub fn select_context_paths_with_skim(context_manager: &ContextManager) -> Result<Option<(Vec<String>, bool)>> {
    let mut all_paths = Vec::new();

    // Get profile-specific paths
    for path in &context_manager.paths {
        all_paths.push(format!(
            "(agent: {}) {}",
            context_manager.current_profile,
            path.get_path_as_str()
        ));
    }

    if all_paths.is_empty() {
        return Ok(None); // No paths to select
    }

    // Create skim options
    let options = create_skim_options("Select paths to remove: ", true)?;

    // Create item reader
    let item_reader = SkimItemReader::default();
    let items = item_reader.of_bufread(Cursor::new(all_paths.join("\n")));

    // Run skim and get selected paths
    match run_skim_with_options(&options, items)? {
        Some(items) if !items.is_empty() => {
            let selected_paths = extract_selections(items);

            // Check if any global paths were selected
            let has_global = selected_paths.iter().any(|p| p.starts_with("(global)"));

            // Extract the actual paths from the formatted strings
            let paths: Vec<String> = selected_paths
                .iter()
                .map(|p| {
                    // Extract the path part after the prefix
                    let parts: Vec<&str> = p.splitn(2, ") ").collect();
                    if parts.len() > 1 {
                        parts[1].to_string()
                    } else {
                        p.clone()
                    }
                })
                .collect();

            Ok(Some((paths, has_global)))
        },
        _ => Ok(None), // User cancelled selection
    }
}

/// Launch the command selector and handle the selected command
pub fn select_command(_os: &Os, context_manager: &ContextManager, tools: &[String]) -> Result<Option<String>> {
    let commands = get_available_commands();

    match launch_skim_selector(&commands, "Select command: ", false)? {
        Some(selections) if !selections.is_empty() => {
            let selected_command = &selections[0];

            match CommandType::from_str(selected_command) {
                Some(CommandType::ContextAdd(cmd)) => {
                    // For context add commands, we need to select files
                    match select_files_with_skim()? {
                        Some(files) if !files.is_empty() => {
                            // Construct the full command with selected files
                            let mut cmd = cmd.clone();
                            for file in files {
                                cmd.push_str(&format!(" {}", file));
                            }
                            Ok(Some(cmd))
                        },
                        _ => Ok(Some(selected_command.clone())), /* User cancelled file selection, return just the
                                                                  * command */
                    }
                },
                Some(CommandType::ContextRemove(cmd)) => {
                    // For context rm commands, we need to select from existing context paths
                    match select_context_paths_with_skim(context_manager)? {
                        Some((paths, has_global)) if !paths.is_empty() => {
                            // Construct the full command with selected paths
                            let mut full_cmd = cmd.clone();
                            if has_global {
                                full_cmd.push_str(" --global");
                            }
                            for path in paths {
                                full_cmd.push_str(&format!(" {}", path));
                            }
                            Ok(Some(full_cmd))
                        },
                        Some((_, _)) => Ok(Some(format!("{} (No paths selected)", cmd))),
                        None => Ok(Some(selected_command.clone())), // User cancelled path selection
                    }
                },
                Some(CommandType::Tools(_)) => {
                    let options = create_skim_options("Select tool: ", false)?;
                    let item_reader = SkimItemReader::default();
                    let items = item_reader.of_bufread(Cursor::new(tools.join("\n")));
                    let selected_tool = match run_skim_with_options(&options, items)? {
                        Some(items) if !items.is_empty() => Some(items[0].output().to_string()),
                        _ => None,
                    };

                    match selected_tool {
                        Some(tool) => Ok(Some(format!("{} {}", selected_command, tool))),
                        None => Ok(Some(selected_command.clone())), /* User cancelled tool selection, return just the
                                                                     * command */
                    }
                },
                Some(cmd @ CommandType::Agent(_)) if cmd.needs_agent_selection() => {
                    // For profile operations that need a profile name, show profile selector
                    // As part of the agent implementation, we are disabling the ability to
                    // switch profile after a session has started.
                    // TODO: perhaps revive this after we have a decision on profile switching
                    Ok(Some(selected_command.clone()))
                },
                Some(CommandType::Agent(_)) => {
                    // For other profile operations (like create), just return the command
                    Ok(Some(selected_command.clone()))
                },
                None => {
                    // Command doesn't need additional parameters
                    Ok(Some(selected_command.clone()))
                },
            }
        },
        _ => Ok(None), // User cancelled command selection
    }
}

/// Select a prompt using skim interface
pub fn select_prompt_with_skim(
    sender: &super::prompt::PromptQuerySender, 
    receiver: &Arc<Mutex<super::prompt::PromptQueryResponseReceiver>>
) -> Result<Option<String>> {
    // Get formatted prompts using the extracted function
    let formatted_prompts = get_available_prompts(sender, receiver)?;
    
    if formatted_prompts.is_empty() {
        return Ok(None);
    }

    // Create display lines with truncation
    let display_lines: Vec<String> = formatted_prompts
        .iter()
        .map(|(prompt_name, server, description, args)| {
            let mut truncated_desc = description.clone();
            truncate_safe_in_place(&mut truncated_desc, PROMPT_SELECTOR_DESC_WIDTH, TRUNCATED_SUFFIX);
            
            let mut truncated_server = server.clone();
            truncate_safe_in_place(&mut truncated_server, PROMPT_SELECTOR_SERVER_WIDTH, TRUNCATED_SUFFIX);
            
            // Arguments get remaining space (no truncation)
            format!("{:<prompt_width$} {:<server_width$} {:<desc_width$} {}", 
                prompt_name, truncated_server, truncated_desc, args,
                prompt_width = PROMPT_SELECTOR_PROMPT_WIDTH,
                server_width = PROMPT_SELECTOR_SERVER_WIDTH,
                desc_width = PROMPT_SELECTOR_DESC_WIDTH)
        })
        .collect();

    // Create prompt mapping for selection
    let prompt_map: std::collections::HashMap<String, String> = display_lines
        .iter()
        .zip(formatted_prompts.iter().map(|(prompt_name, _, _, _)| prompt_name.clone()))
        .map(|(line, prompt)| (line.clone(), prompt))
        .collect();

    // Create skim options with proper header alignment
    let header = format!("{:<prompt_width$} {:<server_width$} {:<desc_width$} {}",
        "Prompt", "Server", "Description", "Arguments (* = required)",
        prompt_width = PROMPT_SELECTOR_PROMPT_WIDTH,
        server_width = PROMPT_SELECTOR_SERVER_WIDTH,
        desc_width = PROMPT_SELECTOR_DESC_WIDTH);
    
    let options = SkimOptionsBuilder::default()
        .height("100%".to_string())
        .prompt("Select prompt: ".to_string())
        .reverse(true)
        .multi(false)
        .header(Some(header))
        .build()
        .map_err(|e| eyre!("Failed to build skim options: {}", e))?;

    let item_reader = SkimItemReader::default();
    let items = item_reader.of_bufread(Cursor::new(display_lines.join("\n")));

    // Use existing helper for consistency
    match run_skim_with_options(&options, items)? {
        Some(items) if !items.is_empty() => {
            let selection = items[0].output().to_string();
            Ok(prompt_map.get(&selection).cloned())
        },
        _ => Ok(None),
    }
}

#[derive(PartialEq)]
enum CommandType {
    ContextAdd(String),
    ContextRemove(String),
    Tools(&'static str),
    Agent(&'static str),
}

impl CommandType {
    fn needs_agent_selection(&self) -> bool {
        matches!(self, CommandType::Agent("set" | "delete" | "rename"))
    }

    fn from_str(cmd: &str) -> Option<CommandType> {
        if cmd.starts_with("/context add") {
            Some(CommandType::ContextAdd(cmd.to_string()))
        } else if cmd.starts_with("/context rm") {
            Some(CommandType::ContextRemove(cmd.to_string()))
        } else {
            match cmd {
                "/tools trust" => Some(CommandType::Tools("trust")),
                "/tools untrust" => Some(CommandType::Tools("untrust")),
                "/agent set" => Some(CommandType::Agent("set")),
                "/agent delete" => Some(CommandType::Agent("delete")),
                "/agent rename" => Some(CommandType::Agent("rename")),
                "/agent create" => Some(CommandType::Agent("create")),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Test to verify that all hardcoded command strings in select_command
    /// are present in the COMMANDS array from prompt.rs
    #[test]
    fn test_hardcoded_commands_in_commands_array() {
        // Get the set of available commands from prompt.rs
        let available_commands: HashSet<String> = get_available_commands().iter().cloned().collect();

        // List of hardcoded commands used in select_command
        let hardcoded_commands = vec![
            "/context add",
            "/context rm",
            "/tools trust",
            "/tools untrust",
            "/agent set",
            "/agent delete",
            "/agent rename",
            "/agent create",
        ];

        // Check that each hardcoded command is in the COMMANDS array
        for cmd in hardcoded_commands {
            assert!(
                available_commands.contains(cmd),
                "Command '{}' is used in select_command but not defined in COMMANDS array",
                cmd
            );

            // This should assert that all the commands we assert are present in the match statement of
            // select_command()
            assert!(
                CommandType::from_str(cmd).is_some(),
                "Command '{}' cannot be parsed into a CommandType",
                cmd
            );
        }
    }

    #[test]
    fn test_format_prompt_arguments() {
        use super::super::tool_manager::PromptBundle;
        use amzn_mcp_client::types::{Prompt, PromptArgument};

        // Test with no arguments
        let bundle_no_args = PromptBundle {
            server_name: "test".to_string(),
            prompt_get: Prompt {
                name: "test".to_string(),
                description: Some("test".to_string()),
                arguments: None,
            },
        };
        assert_eq!(bundle_no_args.format_arguments_for_display(), "");

        // Test with empty arguments
        let bundle_empty_args = PromptBundle {
            server_name: "test".to_string(),
            prompt_get: Prompt {
                name: "test".to_string(),
                description: Some("test".to_string()),
                arguments: Some(vec![]),
            },
        };
        assert_eq!(bundle_empty_args.format_arguments_for_display(), "");

        // Test with mixed required and optional arguments
        let bundle_mixed_args = PromptBundle {
            server_name: "test".to_string(),
            prompt_get: Prompt {
                name: "test".to_string(),
                description: Some("test".to_string()),
                arguments: Some(vec![
                    PromptArgument {
                        name: "required_arg".to_string(),
                        description: Some("A required argument".to_string()),
                        required: Some(true),
                    },
                    PromptArgument {
                        name: "optional_arg".to_string(),
                        description: Some("An optional argument".to_string()),
                        required: Some(false),
                    },
                    PromptArgument {
                        name: "default_arg".to_string(),
                        description: Some("Argument with default required".to_string()),
                        required: None, // Should default to false
                    },
                ]),
            },
        };
        assert_eq!(bundle_mixed_args.format_arguments_for_display(), "required_arg*, optional_arg, default_arg");
    }

    #[test]
    fn test_server_name_truncation() {
        use super::util::truncate_safe_in_place;
        
        // Test that long server names are properly truncated in display
        let long_server_name = "very-long-server-name-that-exceeds-column-width";
        assert!(long_server_name.len() > PROMPT_SELECTOR_SERVER_WIDTH);
        
        let mut truncated = long_server_name.to_string();
        truncate_safe_in_place(&mut truncated, PROMPT_SELECTOR_SERVER_WIDTH, TRUNCATED_SUFFIX);
        
        assert_eq!(truncated, "very-...");  // Updated for 8-char width
        assert_eq!(truncated.len(), PROMPT_SELECTOR_SERVER_WIDTH); // Fits in column width
    }

    #[test]
    fn test_column_constants() {
        use super::util::truncate_safe_in_place;
        
        // Verify our constants are reasonable values
        assert_eq!(PROMPT_SELECTOR_SERVER_WIDTH, 8);
        assert_eq!(PROMPT_SELECTOR_PROMPT_WIDTH, 18);
        assert_eq!(PROMPT_SELECTOR_DESC_WIDTH, 25);
        // Arguments column has no fixed width - uses remaining space
        
        // Verify truncation logic uses constants correctly
        let long_desc = "a".repeat(PROMPT_SELECTOR_DESC_WIDTH + 10);
        let mut truncated_desc = long_desc.clone();
        truncate_safe_in_place(&mut truncated_desc, PROMPT_SELECTOR_DESC_WIDTH, TRUNCATED_SUFFIX);
        assert_eq!(truncated_desc.len(), PROMPT_SELECTOR_DESC_WIDTH);
    }

    #[test]
    fn test_prompt_selector_new() {
        use tokio::sync::broadcast;
        use super::super::tool_manager::{PromptQuery, PromptQueryResult};

        let (sender, _) = broadcast::channel::<PromptQuery>(5);
        let (_, receiver) = broadcast::channel::<PromptQueryResult>(5);
        
        let selector = PromptSelector::new(sender, receiver);
        
        // Test that the selector was created successfully
        // We can't easily test the internal state, but we can verify it doesn't panic
        assert!(true); // If we get here, the constructor worked
    }
}
