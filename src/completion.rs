use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Arg, ArgAction, Command as ClapCommand, CommandFactory};

use crate::cli::{Cli, CompletionShell};
use crate::config::{load_config, resolve_config_path};

pub fn normalize_shell(shell: Option<CompletionShell>) -> Result<CompletionShell> {
    if let Some(shell) = shell {
        return Ok(shell);
    }
    let detected = std::env::var("SHELL")
        .ok()
        .and_then(|shell| Path::new(&shell).file_name().map(|name| name.to_owned()))
        .and_then(|name| name.to_str().map(str::to_string));
    match detected.as_deref() {
        Some("bash") => Ok(CompletionShell::Bash),
        Some("zsh") => Ok(CompletionShell::Zsh),
        Some("fish") => Ok(CompletionShell::Fish),
        _ => Err(anyhow!(
            "could not detect shell from SHELL={}; pass bash, zsh, or fish",
            detected.as_deref().unwrap_or("(unset)")
        )),
    }
}

pub fn completion_instructions(shell: CompletionShell) -> String {
    let shell_name = shell_name(shell);
    let current_shell_command = match shell {
        CompletionShell::Fish => "codex-threads completion script fish | source".to_string(),
        CompletionShell::Bash | CompletionShell::Zsh => {
            format!("source <(codex-threads completion script {shell_name})")
        }
    };
    [
        format!("Detected shell: {shell_name}"),
        String::new(),
        "For this shell only:".to_string(),
        format!("  {current_shell_command}"),
        String::new(),
        "To enable permanently, generate a completion file once:".to_string(),
    ]
    .into_iter()
    .chain(
        permanent_completion_commands(shell)
            .into_iter()
            .map(|command| format!("  {command}")),
    )
    .chain([
        String::new(),
        "Regenerate that file after upgrading codex-threads.".to_string(),
        String::new(),
    ])
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn completion_script(shell: CompletionShell) -> String {
    match shell {
        CompletionShell::Bash => bash_completion_script(),
        CompletionShell::Zsh => zsh_completion_script(),
        CompletionShell::Fish => fish_completion_script(),
    }
}

pub fn completion_candidates(prefix: &str, words: &[String]) -> String {
    let root = Cli::command();
    let candidates = resolve_completion_candidates(&root, prefix, words);
    let matches = candidates
        .into_iter()
        .filter(|candidate| candidate.starts_with(prefix))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        String::new()
    } else {
        format!("{}\n", matches.join("\n"))
    }
}

fn resolve_completion_candidates(
    root: &ClapCommand,
    prefix: &str,
    words: &[String],
) -> Vec<String> {
    let context = completion_context(root, words);
    if let Some((flag, option, value_prefix)) = option_value_prefix(context.command, root, prefix) {
        return value_candidates(option, &context, value_prefix)
            .into_iter()
            .map(|value| format!("{flag}={value}"))
            .collect();
    }

    if let Some(option) = context.pending_option {
        return value_candidates(option, &context, prefix);
    }

    if prefix.starts_with('-') {
        return option_candidates(context.command, root);
    }

    let mut candidates = Vec::new();
    if context.operands.is_empty() {
        candidates.extend(subcommand_candidates(context.command));
    }

    if let Some(argument) = positional_argument(context.command, context.operands.len()) {
        candidates.extend(value_candidates(argument, &context, prefix));
    }

    let app_candidates = unique(candidates);
    if app_candidates.is_empty() {
        option_candidates(context.command, root)
    } else {
        app_candidates
    }
}

struct CompletionContext<'a> {
    command: &'a ClapCommand,
    operands: Vec<String>,
    option_values: HashMap<String, Vec<String>>,
    pending_option: Option<&'a Arg>,
}

fn completion_context<'a>(root: &'a ClapCommand, words: &[String]) -> CompletionContext<'a> {
    let mut command = root;
    let mut operands = Vec::new();
    let mut option_values = HashMap::new();
    let mut pending_option = None;

    for (index, word) in words.iter().enumerate() {
        if let Some(option) = pending_option {
            record_option_value(&mut option_values, option, word);
            pending_option = None;
            continue;
        }

        if word == "--" {
            operands.extend(words[index + 1..].iter().cloned());
            break;
        }

        if operands.is_empty()
            && let Some(subcommand) = visible_subcommand(command, word)
        {
            command = subcommand;
            continue;
        }

        if let Some(option) = option_for_token(command, root, word) {
            if option_takes_value(option) {
                if let Some((_, value)) = word.split_once('=') {
                    record_option_value(&mut option_values, option, value);
                } else {
                    pending_option = Some(option);
                }
            }
            continue;
        }

        operands.push(word.clone());
    }

    CompletionContext {
        command,
        operands,
        option_values,
        pending_option,
    }
}

fn option_value_prefix<'a, 'p>(
    command: &'a ClapCommand,
    root: &'a ClapCommand,
    prefix: &'p str,
) -> Option<(String, &'a Arg, &'p str)> {
    if !prefix.starts_with("--") {
        return None;
    }
    let (flag, value_prefix) = prefix.split_once('=')?;
    let option = option_for_long(command, root, flag.trim_start_matches("--"))?;
    if !option_takes_value(option) {
        return None;
    }
    Some((flag.to_string(), option, value_prefix))
}

fn option_for_token<'a>(
    command: &'a ClapCommand,
    root: &'a ClapCommand,
    token: &str,
) -> Option<&'a Arg> {
    if !token.starts_with('-') {
        return None;
    }
    let flag = token
        .split_once('=')
        .map(|(flag, _)| flag)
        .unwrap_or(token)
        .trim_start_matches("--");
    option_for_long(command, root, flag)
}

fn option_for_long<'a>(
    command: &'a ClapCommand,
    root: &'a ClapCommand,
    flag: &str,
) -> Option<&'a Arg> {
    command
        .get_arguments()
        .chain(
            root.get_arguments()
                .filter(|argument| argument.is_global_set()),
        )
        .find(|argument| {
            !argument.is_positional()
                && argument.get_long() == Some(flag)
                && !argument.is_hide_set()
        })
}

fn value_candidates(target: &Arg, context: &CompletionContext<'_>, _prefix: &str) -> Vec<String> {
    if target.get_long() == Some("server") {
        return server_name_candidates(context);
    }

    target
        .get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_string())
        .collect()
}

fn server_name_candidates(context: &CompletionContext<'_>) -> Vec<String> {
    if latest_option_value(&context.option_values, "connect").is_some() {
        return Vec::new();
    }
    let config = latest_option_value(&context.option_values, "config").map(PathBuf::from);
    let path = resolve_config_path(config);
    match load_config(&path) {
        Ok(config) => config.servers.keys().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

fn positional_argument(command: &ClapCommand, operand_index: usize) -> Option<&Arg> {
    command.get_positionals().nth(operand_index)
}

fn option_candidates(command: &ClapCommand, root: &ClapCommand) -> Vec<String> {
    unique(
        command
            .get_arguments()
            .chain(
                root.get_arguments()
                    .filter(|argument| argument.is_global_set()),
            )
            .filter(|argument| !argument.is_positional() && !argument.is_hide_set())
            .filter_map(|argument| argument.get_long().map(|long| format!("--{long}")))
            .collect(),
    )
}

fn subcommand_candidates(command: &ClapCommand) -> Vec<String> {
    command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| subcommand.get_name().to_string())
        .collect()
}

fn visible_subcommand<'a>(command: &'a ClapCommand, name: &str) -> Option<&'a ClapCommand> {
    command
        .get_subcommands()
        .find(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() == name)
}

fn option_takes_value(option: &Arg) -> bool {
    matches!(option.get_action(), ArgAction::Set | ArgAction::Append)
}

fn record_option_value(values: &mut HashMap<String, Vec<String>>, option: &Arg, value: &str) {
    if let Some(long) = option.get_long() {
        values
            .entry(long.to_string())
            .or_default()
            .push(value.to_string());
    }
}

fn latest_option_value<'a>(
    values: &'a HashMap<String, Vec<String>>,
    flag: &str,
) -> Option<&'a str> {
    values
        .get(flag)
        .and_then(|values| values.last().map(String::as_str))
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn shell_name(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Bash => "bash",
        CompletionShell::Zsh => "zsh",
        CompletionShell::Fish => "fish",
    }
}

fn permanent_completion_commands(shell: CompletionShell) -> Vec<&'static str> {
    match shell {
        CompletionShell::Bash => vec![
            "mkdir -p ~/.local/share/codex-threads",
            "codex-threads completion script bash > ~/.local/share/codex-threads/completion.bash",
            "printf '\\nsource ~/.local/share/codex-threads/completion.bash\\n' >> ~/.bashrc",
        ],
        CompletionShell::Zsh => vec![
            "mkdir -p ~/.local/share/codex-threads",
            "codex-threads completion script zsh > ~/.local/share/codex-threads/completion.zsh",
            "printf '\\nsource ~/.local/share/codex-threads/completion.zsh\\n' >> ~/.zshrc",
        ],
        CompletionShell::Fish => vec![
            "mkdir -p ~/.config/fish/completions",
            "codex-threads completion script fish > ~/.config/fish/completions/codex-threads.fish",
        ],
    }
}

fn bash_completion_script() -> String {
    r#"_codex_threads_completion() {
  local cur
  local -a words
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  words=("${COMP_WORDS[@]:1:COMP_CWORD-1}")
  mapfile -t COMPREPLY < <(codex-threads __complete -- "$cur" "${words[@]}" 2>/dev/null)
}

complete -o bashdefault -o default -F _codex_threads_completion codex-threads
"#
    .to_string()
}

fn zsh_completion_script() -> String {
    r#"#compdef codex-threads

_codex_threads() {
  local current="${words[CURRENT]}"
  local -a prior=()
  if (( CURRENT > 2 )); then
    prior=("${words[2,$(( CURRENT - 1 ))]}")
  fi
  local -a names
  names=("${(@f)$(codex-threads __complete -- "$current" "${prior[@]}" 2>/dev/null)}")
  if (( ${#names[@]} )); then
    compadd -a names
  else
    _files
  fi
}

compdef _codex_threads codex-threads
"#
    .to_string()
}

fn fish_completion_script() -> String {
    r#"function __codex_threads_complete
  set -l current (commandline -ct)
  set -l words (commandline -opc)
  if test (count $words) -gt 0
    set -e words[1]
  end
  if test (count $words) -gt 0; and test "$words[-1]" = "$current"
    set -e words[-1]
  end
  codex-threads __complete -- "$current" $words 2>/dev/null
end

complete -c codex-threads -a '(__codex_threads_complete)'
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_top_level_commands_and_hides_helper() {
        let output = completion_candidates("l", &[]);
        assert!(output.contains("list\n"));
        assert!(!output.contains("__complete"));
    }

    #[test]
    fn completes_nested_subcommands() {
        assert_eq!(
            completion_candidates("p", &[String::from("servers")]),
            "ping\n"
        );
        assert_eq!(
            completion_candidates("s", &[String::from("annotate")]),
            "set\nsearch\n"
        );
        assert_eq!(
            completion_candidates("s", &[String::from("completion")]),
            "script\n"
        );
    }

    #[test]
    fn completes_options_and_static_values() {
        assert!(completion_candidates("--so", &[String::from("list")]).contains("--sort\n"));
        assert_eq!(
            completion_candidates("u", &[String::from("list"), String::from("--sort")]),
            "updated\n"
        );
        assert_eq!(
            completion_candidates("h", &[String::from("new"), String::from("--effort")]),
            "high\n"
        );
        assert_eq!(
            completion_candidates(
                "usage",
                &[
                    String::from("goal"),
                    String::from("set"),
                    String::from("--status"),
                ],
            ),
            "usage-limited\n"
        );
        assert_eq!(
            completion_candidates("b", &[String::from("completion"), String::from("script")],),
            "bash\n"
        );
    }
}
