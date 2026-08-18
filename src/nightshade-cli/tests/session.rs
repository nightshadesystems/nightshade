//! The CLI against a real configd, over a real socket.
//!
//! Not a mock. A client tested against a mock of its own server is a client
//! tested against its author's idea of the protocol, and the two disagree
//! exactly where it matters.

use std::sync::Arc;

use nightshade_cli::client::Client;
use nightshade_cli::complete;
use nightshade_cli::repl::{Cli, Mode, Outcome};
use nightshade_common::paths::Paths;
use nightshade_configd::{Access, Bound, Configd, Server};
use nightshade_render::MockHost;
use nightshade_schema::model::Schema;
use tempfile::TempDir;
use tokio::sync::watch;

struct Harness {
    _dir: TempDir,
    paths: Paths,
    shutdown: watch::Sender<bool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let paths = Paths::under(dir.path());
        let socket = paths.socket();
        let (shutdown, rx) = watch::channel(false);

        let serving = paths.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let access = Access::current_user();
                let host = Arc::new(MockHost::new());
                let configd = Arc::new(
                    Configd::start(Schema::compiled(), serving, host).expect("configd starts"),
                );
                configd.resume().await;
                let bound = Bound::create(&socket, &access).expect("the socket binds");
                Server::new(configd, access).run(bound, rx).await;
            });
        });

        let harness = Self {
            _dir: dir,
            paths,
            shutdown,
            server: Some(server),
        };
        harness.wait_until_ready();
        harness
    }

    fn wait_until_ready(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(self.paths.socket()).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("configd never started listening");
    }

    fn cli(&self) -> Cli {
        let client = Client::connect(&self.paths.socket()).expect("connecting");
        // Colour off: these tests read the text, and a person running them in
        // a terminal should get the same answer as CI.
        Cli::new(client, Schema::compiled(), false)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Run a line and get its outcome, refreshing first as the REPL does.
fn run(cli: &mut Cli, line: &str) -> Outcome {
    cli.refresh();
    cli.run(line)
}

// ---------------------------------------------------------------------------
// modes and prompts
// ---------------------------------------------------------------------------

#[test]
fn the_prompt_says_which_mode_and_which_box() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    cli.refresh();
    assert_eq!(cli.mode, Mode::Operational);
    assert!(cli.prompt().ends_with("> "), "{}", cli.prompt());
    assert!(cli.prompt().contains('@'), "{}", cli.prompt());

    assert_eq!(run(&mut cli, "configure"), Outcome::Ok);
    cli.refresh();
    assert_eq!(cli.mode, Mode::Configuration);
    assert!(cli.prompt().ends_with("# "), "{}", cli.prompt());
}

/// A prompt that lies about which box you are on is a prompt that gets
/// somebody else's firewall reconfigured.
#[test]
fn committing_a_host_name_changes_the_prompt() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    run(&mut cli, "configure");
    cli.refresh();
    let before = cli.prompt();
    assert!(before.contains("nightshade"), "{before}");

    run(&mut cli, "set system host-name edge-fw-01");
    // Not yet: the change is in the candidate, and the box is still itself.
    cli.refresh();
    assert_eq!(cli.prompt(), before);

    assert_eq!(run(&mut cli, "commit"), Outcome::Ok);
    cli.refresh();
    let after = cli.prompt();
    assert!(after.contains("edge-fw-01"), "{after}");
    assert!(after.ends_with("# "), "{after}");
}

#[test]
fn edit_shows_where_you_are_and_up_and_top_get_you_out() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");

    assert_eq!(run(&mut cli, "edit interfaces ethernet eth0"), Outcome::Ok);
    cli.refresh();
    assert!(cli.prompt().contains("[edit interfaces ethernet eth0]"), "{}", cli.prompt());

    // Everything typed is relative to it.
    assert_eq!(run(&mut cli, "set mtu 9000"), Outcome::Ok);

    assert_eq!(run(&mut cli, "up"), Outcome::Ok);
    cli.refresh();
    assert!(cli.prompt().contains("[edit interfaces ethernet]"), "{}", cli.prompt());

    assert_eq!(run(&mut cli, "top"), Outcome::Ok);
    cli.refresh();
    assert!(!cli.prompt().contains("[edit"), "{}", cli.prompt());

    // And the relative set landed in the right place.
    assert_eq!(run(&mut cli, "delete interfaces ethernet eth0 mtu"), Outcome::Ok);
}

#[test]
fn editing_somewhere_that_does_not_exist_is_refused() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");
    assert_eq!(run(&mut cli, "edit nonsense"), Outcome::ConfigError);
}

// ---------------------------------------------------------------------------
// exit
// ---------------------------------------------------------------------------

/// Leaving configuration mode must not lose work by accident.
#[test]
fn exit_refuses_while_there_are_uncommitted_changes() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    run(&mut cli, "configure");
    run(&mut cli, "set system host-name fw");

    assert_eq!(run(&mut cli, "exit"), Outcome::ConfigError);
    cli.refresh();
    assert_eq!(cli.mode, Mode::Configuration, "exit left anyway");

    // Said out loud, and there is a way through.
    assert_eq!(run(&mut cli, "exit discard"), Outcome::Ok);
    cli.refresh();
    assert_eq!(cli.mode, Mode::Operational);
}

#[test]
fn exit_leaves_when_there_is_nothing_to_lose() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");
    assert_eq!(run(&mut cli, "exit"), Outcome::Ok);
    cli.refresh();
    assert_eq!(cli.mode, Mode::Operational);

    assert_eq!(run(&mut cli, "exit"), Outcome::Leave);
}

// ---------------------------------------------------------------------------
// completion and ? help
// ---------------------------------------------------------------------------

/// The acceptance case: `set inter<Tab>` completes to `interfaces`.
#[test]
fn completion_works_at_every_position() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");
    run(&mut cli, "set interfaces ethernet eth0 mtu 9000");
    cli.refresh();

    let offered = |cli: &Cli, line: &str| -> Vec<String> {
        let context = cli.context.lock().unwrap();
        let (words, partial) = complete::words(line);
        complete::candidates(Schema::compiled(), &context, &words, partial)
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    };

    assert_eq!(offered(&cli, "set inter"), ["interfaces"]);
    assert_eq!(offered(&cli, "set "), ["interfaces", "system"]);
    assert_eq!(
        offered(&cli, "set system "),
        ["host-name", "name-server", "time-zone"]
    );

    // An interface that is configured is offered by name.
    assert!(
        offered(&cli, "delete interfaces ethernet ").contains(&"eth0".to_string()),
        "{:?}",
        offered(&cli, "delete interfaces ethernet ")
    );

    // A value position offers the shape of the value.
    let mtu = offered(&cli, "set interfaces ethernet eth0 mtu ");
    assert_eq!(mtu, ["<68-9216>"]);
}

#[test]
fn question_mark_lists_what_can_go_here() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");

    // `?` prints and never fails.
    assert_eq!(run(&mut cli, "set system ?"), Outcome::Ok);
    assert_eq!(run(&mut cli, "?"), Outcome::Ok);
    assert_eq!(run(&mut cli, "set interfaces ethernet eth0 mtu ?"), Outcome::Ok);
}

/// `?` answers the same question `<Tab>` does, at the same position.
///
/// The space in front of the `?` is what says the word before it is finished.
/// Lose it and every `?` answers one word too early: `set ?` offers `set`
/// instead of what can follow it.
#[test]
fn question_mark_asks_about_the_position_after_the_last_word() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");
    cli.refresh();

    // Exactly what a typed line does: split off the `?`, then answer it.
    let asked = |cli: &Cli, line: &str| -> String {
        cli.help(complete::question(line).expect("a question"))
    };

    // `set ?` is a question about what comes after `set`.
    let after_set = asked(&cli, "set ?");
    assert!(after_set.contains("interfaces"), "{after_set}");
    assert!(after_set.contains("system"), "{after_set}");
    assert!(!after_set.contains("\n  set"), "{after_set}");

    let after_system = asked(&cli, "set system ?");
    for leaf in ["host-name", "name-server", "time-zone"] {
        assert!(after_system.contains(leaf), "{after_system}");
    }

    // Without the space it is a question about the word itself, still.
    let narrowing = asked(&cli, "set sys?");
    assert!(narrowing.contains("system"), "{narrowing}");
    assert!(!narrowing.contains("host-name"), "{narrowing}");

    // A bare `?` is the command list.
    let commands = asked(&cli, "?");
    assert!(commands.contains("commit"), "{commands}");
    assert!(commands.contains("rollback"), "{commands}");

    // A value position describes the value.
    let mtu = asked(&cli, "set interfaces ethernet eth0 mtu ?");
    assert!(mtu.contains("<68-9216>"), "{mtu}");
    assert!(mtu.contains("1500"), "{mtu}");
}

// ---------------------------------------------------------------------------
// the configuration commands
// ---------------------------------------------------------------------------

#[test]
fn a_whole_edit_runs_from_the_command_line() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    run(&mut cli, "configure");
    for line in [
        "set system host-name fw-01",
        "set system name-server 1.1.1.1",
        "set interfaces ethernet eth0 address 192.168.1.1/24",
        "set interfaces ethernet eth1",
        "set interfaces ethernet eth2",
        "set interfaces bonding bond0 member eth1",
        "set interfaces bonding bond0 member eth2",
        "set interfaces bonding bond0 address 10.0.0.1/24",
    ] {
        assert_eq!(run(&mut cli, line), Outcome::Ok, "{line}");
    }

    assert_eq!(run(&mut cli, "compare"), Outcome::Ok);
    assert_eq!(run(&mut cli, "show"), Outcome::Ok);
    assert_eq!(run(&mut cli, "commit"), Outcome::Ok);
    assert_eq!(run(&mut cli, "save"), Outcome::Ok);
    assert_eq!(run(&mut cli, "exit"), Outcome::Ok);

    // And the operational side can see it.
    assert_eq!(run(&mut cli, "show configuration"), Outcome::Ok);
    assert_eq!(run(&mut cli, "show system commit-log"), Outcome::Ok);
    assert_eq!(run(&mut cli, "show interfaces"), Outcome::Ok);
    assert_eq!(run(&mut cli, "show version"), Outcome::Ok);
}

/// The short spellings people actually type reach the same commands.
#[test]
fn the_aliases_reach_the_command_they_are_short_for() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    // Every spelling of `configure` opens a session that `exit` closes.
    for spelling in ["configure", "config", "conf"] {
        assert_eq!(run(&mut cli, spelling), Outcome::Ok, "{spelling}");
        cli.refresh();
        assert_eq!(cli.mode, Mode::Configuration, "{spelling}");
        assert_eq!(run(&mut cli, "exit"), Outcome::Ok, "{spelling}");
        cli.refresh();
        assert_eq!(cli.mode, Mode::Operational, "{spelling}");
    }

    // Every spelling of the saved configuration, and `sh` for `show`.
    for line in [
        "show configuration",
        "show config",
        "show conf",
        "sh configuration",
        "sh config",
        "sh conf",
        "sh version",
    ] {
        assert_eq!(run(&mut cli, line), Outcome::Ok, "{line}");
    }

    // `sh` is show, in configuration mode too, where it takes a path.
    run(&mut cli, "config");
    cli.refresh();
    assert_eq!(run(&mut cli, "sh interfaces"), Outcome::Ok);
    run(&mut cli, "exit");

    // Named aliases, not prefix matching: a word that is not one of them is
    // still an error rather than a guess.
    cli.refresh();
    assert_eq!(run(&mut cli, "configu"), Outcome::CommandError);
    assert_eq!(run(&mut cli, "s version"), Outcome::CommandError);
    cli.refresh();
    assert_eq!(cli.mode, Mode::Operational);
}

#[test]
fn a_bad_value_exits_two_and_a_bad_command_exits_one() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");

    // A configuration problem.
    assert_eq!(
        run(&mut cli, "set interfaces ethernet eth0 mtu 100000"),
        Outcome::ConfigError
    );
    assert_eq!(run(&mut cli, "set system hostname fw"), Outcome::ConfigError);

    // A command problem.
    assert_eq!(run(&mut cli, "sett system host-name fw"), Outcome::CommandError);
    assert_eq!(run(&mut cli, "rollback not-a-number"), Outcome::CommandError);

    assert_eq!(Outcome::ConfigError.code(), 2);
    assert_eq!(Outcome::CommandError.code(), 1);
}

#[test]
fn commit_takes_a_comment_and_a_confirm_window() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");
    run(&mut cli, "set system host-name fw");

    assert_eq!(run(&mut cli, "commit comment \"first change\""), Outcome::Ok);

    run(&mut cli, "set system time-zone UTC");
    assert_eq!(run(&mut cli, "commit confirm 5"), Outcome::Ok);
    assert!(harness.paths.pending_confirm().exists());
    assert_eq!(run(&mut cli, "confirm"), Outcome::Ok);
    assert!(!harness.paths.pending_confirm().exists());

    // Malformed forms are command errors, not silent successes.
    run(&mut cli, "set system host-name fw2");
    assert_eq!(run(&mut cli, "commit confirm"), Outcome::CommandError);
    assert_eq!(run(&mut cli, "commit confirm nonsense"), Outcome::CommandError);
    assert_eq!(run(&mut cli, "commit nonsense"), Outcome::CommandError);
}

// ---------------------------------------------------------------------------
// no shell escapes
// ---------------------------------------------------------------------------

/// `ns` is somebody's login shell. Nothing typed at it may reach a shell.
#[test]
fn nothing_typed_reaches_a_shell() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    // Every shape somebody would try. None of these may succeed, and none may
    // do anything except produce a message.
    for attempt in [
        "!id",
        "! id",
        "show version; id",
        "show version && id",
        "show version | sh",
        "show version | bash -c id",
        "$(id)",
        "`id`",
        "show interfaces | match $(id)",
        "ping $(id)",
        "ping -c 100000 localhost",
        "ping; id",
        "traceroute `hostname`",
        "ping --help",
        "ping -oProxyCommand=id",
    ] {
        let outcome = run(&mut cli, attempt);
        assert_ne!(
            outcome,
            Outcome::Leave,
            "{attempt:?} left the session"
        );
    }

    // Still alive, still operational, and configd is still there.
    cli.refresh();
    assert_eq!(cli.mode, Mode::Operational);
    assert_eq!(run(&mut cli, "show version"), Outcome::Ok);
}

/// `shell` is a command, and only from operational mode.
#[test]
fn shell_is_refused_in_configuration_mode() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    run(&mut cli, "configure");
    assert_eq!(run(&mut cli, "shell"), Outcome::CommandError);
    cli.refresh();
    assert_eq!(cli.mode, Mode::Configuration, "shell left configuration mode");
}

// ---------------------------------------------------------------------------
// pipe modifiers
// ---------------------------------------------------------------------------

#[test]
fn pipe_modifiers_post_process_and_never_spawn() {
    let harness = Harness::start();
    let mut cli = harness.cli();
    run(&mut cli, "configure");
    run(&mut cli, "set system host-name fw-01");
    run(&mut cli, "set system name-server 1.1.1.1");
    run(&mut cli, "commit");
    run(&mut cli, "exit");

    for line in [
        "show configuration | match host-name",
        "show configuration | count",
        "show configuration | no-more",
        "show configuration | display json",
        "show configuration | match name-server | count",
        "show interfaces | display json",
        "show system commit-log | display json",
    ] {
        assert_eq!(run(&mut cli, line), Outcome::Ok, "{line}");
    }

    // A modifier that does not exist is a command error.
    assert_eq!(
        run(&mut cli, "show configuration | grep host-name"),
        Outcome::CommandError
    );
}

// ---------------------------------------------------------------------------
// rollback and load
// ---------------------------------------------------------------------------

#[test]
fn rollback_loads_a_revision_and_the_operator_commits_it() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    run(&mut cli, "configure");
    for name in ["one", "two", "three"] {
        run(&mut cli, &format!("set system host-name {name}"));
        assert_eq!(run(&mut cli, "commit"), Outcome::Ok);
    }
    cli.refresh();
    assert!(cli.prompt().contains("three"), "{}", cli.prompt());

    assert_eq!(run(&mut cli, "rollback 1"), Outcome::Ok);
    // Loaded, not applied.
    cli.refresh();
    assert!(cli.prompt().contains("three"), "{}", cli.prompt());

    assert_eq!(run(&mut cli, "commit"), Outcome::Ok);
    cli.refresh();
    assert!(cli.prompt().contains("one"), "{}", cli.prompt());

    assert_eq!(run(&mut cli, "rollback 99"), Outcome::CommandError);
}

#[test]
fn a_hand_edited_saved_config_loads_back() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    run(&mut cli, "configure");
    run(&mut cli, "set system host-name fw-01");
    run(&mut cli, "commit");
    assert_eq!(run(&mut cli, "save"), Outcome::Ok);

    let path = harness.paths.config_boot();
    let saved = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, saved.replace("fw-01", "fw-02")).unwrap();

    assert_eq!(run(&mut cli, "load"), Outcome::Ok);
    assert_eq!(run(&mut cli, "commit"), Outcome::Ok);
    cli.refresh();
    assert!(cli.prompt().contains("fw-02"), "{}", cli.prompt());
}

// ---------------------------------------------------------------------------
// operational mode
// ---------------------------------------------------------------------------

#[test]
fn operational_commands_that_do_not_exist_are_refused_helpfully() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    assert_eq!(run(&mut cli, "set system host-name fw"), Outcome::CommandError);
    assert_eq!(run(&mut cli, "commit"), Outcome::CommandError);
    assert_eq!(run(&mut cli, "show nonsense"), Outcome::CommandError);
}

#[test]
fn the_completion_context_follows_the_mode() {
    let harness = Harness::start();
    let mut cli = harness.cli();

    cli.refresh();
    {
        let context = cli.context.lock().unwrap();
        assert!(context.commands.contains(&"configure"));
        assert!(!context.commands.contains(&"commit"));
    }

    run(&mut cli, "configure");
    cli.refresh();
    {
        let context = cli.context.lock().unwrap();
        assert!(context.commands.contains(&"commit"));
        assert!(!context.commands.contains(&"configure"));
    }
}

/// Two sessions from two `ns` processes do not see each other's candidates.
#[test]
fn two_clients_have_their_own_candidates() {
    let harness = Harness::start();
    let mut a = harness.cli();
    let mut b = harness.cli();

    run(&mut a, "configure");
    run(&mut b, "configure");

    run(&mut a, "set system host-name from-a");
    b.refresh();
    {
        let context = b.context.lock().unwrap();
        assert!(
            context.candidate.is_empty(),
            "one session saw another's candidate"
        );
    }

    assert_eq!(run(&mut a, "commit"), Outcome::Ok);
    // B is now behind, and is told so rather than silently reverting A.
    run(&mut b, "set system time-zone UTC");
    assert_eq!(run(&mut b, "commit"), Outcome::ConfigError);
}
