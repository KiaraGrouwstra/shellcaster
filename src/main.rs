use std::process;
use std::path::PathBuf;
use std::sync::mpsc;

use clap::{App, Arg, SubCommand};

mod main_controller;
mod config;
mod keymap;
mod db;
mod ui;
mod types;
mod threadpool;
mod feeds;
mod sanitizer;
mod downloads;
mod play_file;

use crate::main_controller::{MainController, MainMessage};
use crate::config::Config;
use crate::db::Database;
use crate::threadpool::Threadpool;
use crate::types::*;
use crate::feeds::FeedMsg;

/// Main controller for shellcaster program.
/// 
/// *Main command:*
/// Setup involves connecting to the sqlite database (creating it if 
/// necessary), then querying the list of podcasts and episodes. This
/// is then passed off to the UI, which instantiates the menus displaying
/// the podcast info.
/// 
/// After this, the program enters a loop that listens for user keyboard
/// input, and dispatches to the proper module as necessary. User input
/// to quit the program breaks the loop, tears down the UI, and ends the
/// program.
/// 
/// *Sync subcommand:*
/// Connects to the sqlite database, then initiates a full sync of all
/// podcasts. No UI is created for this, as the intention is to be used
/// in a programmatic way (e.g., setting up a cron job to sync
/// regularly.)
/// 
/// *Import subcommand:*
/// Reads in an OPML file and adds feeds to the database that do not
/// already exist. If the `-r` option is used, the database is wiped
/// first.
/// 
/// *Export subcommand:*
/// Connects to the sqlite database, and reads all podcasts into an OPML
/// file, with the location specified from the command line arguments.
#[allow(clippy::while_let_on_iterator)]
fn main() {
    // SETUP -----------------------------------------------------------

    // set up the possible command line arguments and subcommands
    let args = App::new(clap::crate_name!())
        .version(clap::crate_version!())
        // .author(clap::crate_authors!(", "))
        .author("Jeff Hughes <jeff.hughes@gmail.com>")
        .about(clap::crate_description!())
        .arg(Arg::with_name("config")
            .short("c")
            .long("config")
            .env("SHELLCASTER_CONFIG")
            .global(true)
            .takes_value(true)
            .value_name("FILE")
            .help("Sets a custom config file location. Can also be set with environment variable."))
        .subcommand(SubCommand::with_name("sync")
            .about("Syncs all podcasts in database")
            .arg(Arg::with_name("quiet")
                .short("q")
                .long("quiet")
                .help("Suppresses output messages to stdout.")))
        .subcommand(SubCommand::with_name("import")
            .about("Imports podcasts from an OPML file")
            .arg(Arg::with_name("file")
                .required(true)
                .takes_value(true)
                .value_name("FILE")
                .help("Specifies the filepath to the OPML file to be imported."))
            .arg(Arg::with_name("replace")
                .short("r")
                .long("replace")
                .takes_value(false)
                .help("If set, the contents of the OPML file will replace all existing data in the shellcaster database.")))
        .subcommand(SubCommand::with_name("export")
            .about("Exports podcasts to an OPML file")
            .arg(Arg::with_name("file")
                .required(true)
                .takes_value(true)
                .value_name("FILE")
                .help("Specifies the filepath for where the OPML file will be exported.")))
        .get_matches();

    // figure out where config file is located -- either specified from
    // command line args, set via $SHELLCASTER_CONFIG, or using default
    // config location for OS
    let config_path = get_config_path(args.value_of("config"))
        .unwrap_or_else(|| {
            println!("Could not identify your operating system's default directory to store configuration files. Please specify paths manually using config.toml and use `-c` or `--config` flag to specify where config.toml is located when launching the program.");
            process::exit(1);
        });
    let config = Config::new(&config_path);

    let mut db_path = config_path;
    if !db_path.pop() {
        println!("Could not correctly parse the config file location. Please specify a valid path to the config file.");
        process::exit(1);
    }


    match args.subcommand() {
        // SYNC SUBCOMMAND ----------------------------------------------
        ("sync", Some(sub_args)) => {
            sync_podcasts(&db_path, config, sub_args);
        },


        // IMPORT SUBCOMMAND --------------------------------------------
        ("import", Some(_sub_args)) => {
            todo!();
        },


        // EXPORT SUBCOMMAND --------------------------------------------
        ("export", Some(_sub_args)) => {
            todo!();
        },


        // MAIN COMMAND -------------------------------------------------
        _ => {
            let mut main_ctrl = MainController::new(config, &db_path);

            main_ctrl.loop_msgs();  // main loop

            main_ctrl.tx_to_ui.send(MainMessage::UiTearDown).unwrap();
            main_ctrl.ui_thread.join().unwrap();  // wait for UI thread to finish teardown
        }
    }
}


/// Gets the path to the config file if one is specified in the command-
/// line arguments, or else returns the default config path for the
/// user's operating system.
/// Returns None if default OS config directory cannot be determined.
/// 
/// Note: Right now we only have one possible command-line argument,
/// specifying a config path. If the command-line API is
/// extended in the future, this will have to be refactored.
fn get_config_path(config: Option<&str>) -> Option<PathBuf> {
    return match config {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            let default_config = dirs::config_dir();
            match default_config {
                Some(mut path) => {
                    path.push("shellcaster");
                    path.push("config.toml");
                    Some(path)
                },
                None => None,
            } 
        },
    };
}


/// Synchronizes RSS feed data for all podcasts, without setting up a UI.
fn sync_podcasts(db_path: &PathBuf, config: Config, args: &clap::ArgMatches) {
    let db_inst = Database::connect(db_path);
    let podcast_list = db_inst.get_podcasts();

    if podcast_list.is_empty() {
        if !args.is_present("quiet") {
            println!("No podcasts to sync.");
        }
    } else {
        let threadpool = Threadpool::new(config.simultaneous_downloads);
        let (tx_to_main, rx_to_main) = mpsc::channel();

        for pod in podcast_list.iter() {
            feeds::check_feed(pod.url.clone(), pod.id,
                config.max_retries, &threadpool, tx_to_main.clone());
        }

        let mut msg_counter: usize = 0;
        let mut failure = false;
        while let Some(message) = rx_to_main.iter().next() {
            match message {
                Message::Feed(FeedMsg::SyncData(pod)) => {
                    let title = pod.title.clone();
                    let db_result;
            
                    db_result = db_inst.update_podcast(pod);
                    match db_result {
                        Ok(_) => {
                            if !args.is_present("quiet") {
                                println!("Synced {}", title);
                            }
                        },
                        Err(_err) => {
                            failure = true;
                            eprintln!("Error synchronizing {}", title);
                        },
                    }
                }

                Message::Feed(FeedMsg::Error(pod_id)) => {
                    failure = true;
                    let mut title = None;
                    if let Some(id) = pod_id {
                        for pod in podcast_list.iter() {
                            if let Some(pid) = pod.id {
                                if pid == id {
                                    title = Some(pod.title.clone());
                                    break;
                                }
                            }
                        }
                    }

                    match title {
                        Some(t) => eprintln!("Error retrieving RSS feed for {}.", t),
                        None => eprintln!("Error retrieving RSS feed."),
                    }
                }
                _ => (),
            }

            msg_counter += 1;
            if msg_counter >= podcast_list.len() {
                break;
            }
        }

        if failure {
            eprintln!("Process finished with errors.");
            process::exit(2);
        } else if !args.is_present("quiet") {
            println!("Sync successful.");
        }
    }
}