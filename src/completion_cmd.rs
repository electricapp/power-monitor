//! `power-monitor completion <bash|zsh|fish>` — print a shell completion
//! script to stdout. Hand-rolled (no clap) to keep the crate dependency-free;
//! the command/flag lists below must be kept in sync with the parsers.

use std::io::Write;

pub(crate) fn write_usage(w: &mut impl Write) {
    let _ = writeln!(w, "Usage: power-monitor completion <bash|zsh|fish>");
    let _ = writeln!(w);
    let _ = writeln!(w, "Print a shell completion script to stdout.");
    let _ = writeln!(w);
    let _ = writeln!(w, "Examples:");
    let _ = writeln!(
        w,
        "  power-monitor completion bash > $(brew --prefix)/etc/bash_completion.d/power-monitor"
    );
    let _ = writeln!(
        w,
        "  power-monitor completion zsh  > ~/.zsh/completions/_power-monitor"
    );
    let _ = writeln!(
        w,
        "  power-monitor completion fish > ~/.config/fish/completions/power-monitor.fish"
    );
}

pub fn run(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("bash") => print!("{BASH}"),
        Some("zsh") => print!("{ZSH}"),
        Some("fish") => print!("{FISH}"),
        Some("-h") | Some("--help") => write_usage(&mut std::io::stdout().lock()),
        other => {
            match other {
                Some(s) => eprintln!("error: unknown shell '{s}' (expected bash, zsh, or fish)"),
                None => eprintln!("error: completion requires a shell: bash, zsh, or fish"),
            }
            write_usage(&mut std::io::stderr().lock());
            std::process::exit(2);
        }
    }
}

const BASH: &str = r#"# bash completion for power-monitor
_power_monitor() {
    local cur cmds
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    cmds="pipe serve collect fan doctor man completion help"

    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "$cmds -h --help -V --version --no-color" -- "$cur") )
        return 0
    fi

    case "${COMP_WORDS[1]}" in
        pipe)       COMPREPLY=( $(compgen -W "-s --samples -i --interval -h --help" -- "$cur") );;
        serve)      COMPREPLY=( $(compgen -W "--bind -p --port -i --interval --auth --auth-file --install --uninstall -h --help" -- "$cur") );;
        collect)    COMPREPLY=( $(compgen -W "--host --tailnet -p --port -i --interval --auth --auth-file --install --uninstall -h --help" -- "$cur") );;
        fan)        COMPREPLY=( $(compgen -W "max auto -h --help" -- "$cur") );;
        man)        COMPREPLY=( $(compgen -W "--install -h --help" -- "$cur") );;
        completion) COMPREPLY=( $(compgen -W "bash zsh fish -h --help" -- "$cur") );;
        doctor)     COMPREPLY=( $(compgen -W "--no-color -h --help" -- "$cur") );;
        help)       COMPREPLY=( $(compgen -W "$cmds" -- "$cur") );;
    esac
    return 0
}
complete -F _power_monitor power-monitor
"#;

const ZSH: &str = r#"#compdef power-monitor
_power_monitor() {
    local -a commands
    commands=(
        'pipe:Stream metrics to stdout as NDJSON'
        'serve:Serve JSON + Prometheus metrics over HTTP'
        'collect:Aggregate many agents into one fleet dashboard'
        'fan:Control fan speed (requires root)'
        'doctor:Run health checks'
        'man:Print or install the man page'
        'completion:Generate shell completions'
        'help:Show help'
    )
    if (( CURRENT == 2 )); then
        _describe -t commands 'power-monitor command' commands
        _arguments '-h[help]' '--help[help]' '-V[version]' '--version[version]' '--no-color[disable color]'
        return
    fi
    case "${words[2]}" in
        pipe)    _arguments '(-s --samples)'{-s,--samples}'[stop after N samples]:N' '(-i --interval)'{-i,--interval}'[sampling window ms]:ms' ;;
        serve)   _arguments '--bind[bind address]:addr' '(-p --port)'{-p,--port}'[port]:port' '(-i --interval)'{-i,--interval}'[interval ms]:ms' '--auth[bearer token]:token' '--auth-file[token file]:file:_files' '--install' '--uninstall' ;;
        collect) _arguments '--host[host list]:hosts' '--tailnet[discover via tailscale]' '(-p --port)'{-p,--port}'[port]:port' '(-i --interval)'{-i,--interval}'[interval ms]:ms' '--auth[bearer token]:token' '--auth-file[token file]:file:_files' '--install' '--uninstall' ;;
        fan)     _values 'mode' max auto ;;
        completion) _values 'shell' bash zsh fish ;;
        man)     _arguments '--install[install dir]:dir:_files -/' ;;
        doctor)  _arguments '--no-color[disable color]' ;;
    esac
}
_power_monitor "$@"
"#;

const FISH: &str = r#"# fish completion for power-monitor
complete -c power-monitor -f
set -l cmds pipe serve collect fan doctor man completion help
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a pipe       -d 'Stream NDJSON metrics'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a serve      -d 'HTTP + Prometheus'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a collect    -d 'Fleet dashboard'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a fan        -d 'Fan control (root)'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a doctor     -d 'Health checks'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a man        -d 'Man page'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a completion -d 'Shell completions'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -a help       -d 'Show help'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -s h -l help    -d 'Print help'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -s V -l version -d 'Print version'
complete -c power-monitor -n "not __fish_seen_subcommand_from $cmds" -l no-color     -d 'Disable color'
complete -c power-monitor -n "__fish_seen_subcommand_from fan"        -a 'max auto'
complete -c power-monitor -n "__fish_seen_subcommand_from completion" -a 'bash zsh fish'
"#;
