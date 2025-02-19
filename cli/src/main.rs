use {
    clap::{
        crate_description, crate_name, crate_version, value_t_or_exit, App, AppSettings, Arg,
        SubCommand,
    },
    registry_cli::{get_participants, get_participants_with_state},
    registry_program::state::{Participant, ParticipantState},
    solana_clap_utils::{
        input_parsers::{pubkey_of, signer_of},
        input_validators::{is_url, is_valid_pubkey, is_valid_signer},
        keypair::DefaultSigner,
    },
    solana_client::rpc_client::RpcClient,
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        message::Message,
        native_token::Sol,
        program_pack::Pack,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        signers::Signers,
        system_instruction,
        transaction::Transaction,
    },
    std::{
        collections::{HashMap, HashSet},
        ops::Deref,
        process::exit,
        sync::Arc,
    },
};

struct Config {
    default_signer: Box<dyn Signer>,
    json_rpc_url: String,
    verbose: bool,
}

fn send_and_confirm_message<T: Signers>(
    rpc_client: &RpcClient,
    message: Message,
    signers: T,
    additional_funds_required: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let fee_payer = message.account_keys[0];
    let (recent_blockhash, fee_calculator) = rpc_client
        .get_recent_blockhash()
        .map_err(|err| format!("error: unable to get recent blockhash: {}", err))?;
    let funds_required =
        fee_calculator.calculate_fee(&message) + additional_funds_required.unwrap_or_default();

    let balance = rpc_client.get_balance(&fee_payer)?;

    if balance < funds_required {
        return Err(format!(
            "{} has insufficient balance. {} required",
            fee_payer,
            Sol(funds_required)
        )
        .into());
    }

    let mut transaction = Transaction::new_unsigned(message);
    transaction
        .try_sign(&signers, recent_blockhash)
        .map_err(|err| format!("error: failed to sign transaction: {}", err))?;

    let signature = rpc_client
        .send_and_confirm_transaction_with_spinner(&transaction)
        .map_err(|err| format!("error: send transaction: {}", err))?;

    println!("{}", signature);
    Ok(())
}

fn get_participants_with_identity(
    rpc_client: &RpcClient,
    identities: HashSet<&Pubkey>,
) -> Result<HashMap<Pubkey, Participant>, Box<dyn std::error::Error>> {
    let mut participants = get_participants(rpc_client)?;
    participants.retain(|_, p| {
        identities.contains(&p.testnet_identity) || identities.contains(&p.mainnet_identity)
    });
    Ok(participants)
}

fn get_participant_by_identity(
    rpc_client: &RpcClient,
    identity: Pubkey,
) -> Result<Option<(Pubkey, Participant)>, Box<dyn std::error::Error>> {
    let participant = get_participants(rpc_client)?
        .into_iter()
        .filter(|(_, p)| p.testnet_identity == identity || p.mainnet_identity == identity)
        .collect::<Vec<_>>();

    if participant.len() > 1 {
        Err(format!("{} matches multiple participants", identity).into())
    } else {
        Ok(participant.into_iter().next())
    }
}

fn print_participant(participant: &Participant) {
    println!("State: {:?}", participant.state);
    println!(
        "Mainnet Validator Identity: {}",
        participant.mainnet_identity
    );
    println!(
        "Testnet Validator Identity: {}",
        participant.testnet_identity
    );
}

fn process_status(
    _config: &Config,
    rpc_client: &RpcClient,
    identity: Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    match get_participant_by_identity(rpc_client, identity)? {
        Some((_, participant)) => {
            print_participant(&participant);
        }
        None => {
            println!("Registration not found for {}", identity);
        }
    }
    Ok(())
}

fn process_apply(
    config: &Config,
    rpc_client: &RpcClient,
    mainnet_identity: Box<dyn Signer>,
    testnet_identity: Box<dyn Signer>,
    confirm: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let participants = get_participants_with_identity(
        rpc_client,
        [mainnet_identity.pubkey(), testnet_identity.pubkey()]
            .iter()
            .collect::<HashSet<_>>(),
    )?;

    if !participants.is_empty() {
        return Err("Registration already exists".into());
    }

    println!("Mainnet Validator Identity: {}", mainnet_identity.pubkey());
    println!("Testnet Validator Identity: {}", testnet_identity.pubkey());

    if !confirm {
        println!(
            "\nWarning: Your mainnet and testnet identities cannot be changed after applying. \
                    Add the --confirm flag to continue"
        );
        return Ok(());
    }

    let rent = rpc_client.get_minimum_balance_for_rent_exemption(Participant::get_packed_len())?;
    let participant: Box<dyn Signer> = Box::new(Keypair::new());

    let message = Message::new(
        &[
            system_instruction::create_account(
                &config.default_signer.pubkey(),
                &participant.pubkey(),
                rent,
                Participant::get_packed_len() as u64,
                &registry_program::id(),
            ),
            registry_program::instruction::apply(
                participant.pubkey(),
                mainnet_identity.pubkey(),
                testnet_identity.pubkey(),
            ),
        ],
        Some(&config.default_signer.pubkey()),
    );

    send_and_confirm_message(
        rpc_client,
        message,
        [
            participant.deref(),
            mainnet_identity.deref(),
            testnet_identity.deref(),
            config.default_signer.deref(),
        ],
        Some(rent),
    )
}

fn process_withdraw(
    config: &Config,
    rpc_client: &RpcClient,
    identity: Box<dyn Signer>,
    confirm: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (participant_address, participant) =
        get_participant_by_identity(rpc_client, identity.pubkey())?
            .ok_or_else(|| format!("Registration not found for {}", identity.pubkey()))?;

    print_participant(&participant);

    if !confirm {
        println!(
            "\nWarning: this will delete this registration. \
               Add the --confirm flag to continue"
        );
        return Ok(());
    }

    let message = Message::new(
        &[registry_program::instruction::withdraw(
            participant_address,
            identity.pubkey(),
            config.default_signer.pubkey(),
        )],
        Some(&config.default_signer.pubkey()),
    );

    send_and_confirm_message(
        rpc_client,
        message,
        [identity.deref(), config.default_signer.deref()],
        None,
    )
}

fn process_list(
    config: &Config,
    rpc_client: &RpcClient,
    state: Option<ParticipantState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let participants = get_participants_with_state(rpc_client, state)?;

    for (address, participant) in &participants {
        if config.verbose {
            println!("Participant: {}", address);
        }
        print_participant(participant);
        println!();
    }

    println!("{} entries found", participants.len());
    Ok(())
}

fn process_admin_approve(
    config: &Config,
    rpc_client: &RpcClient,
    admin_signer: Box<dyn Signer>,
    participant_address: Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let participants = get_participants(rpc_client)?;
    let participant = participants
        .get(&participant_address)
        .ok_or_else(|| format!("Participant {} does not exist", participant_address))?;

    print_participant(&participant);
    println!("Approving...");

    let message = Message::new(
        &[registry_program::instruction::approve(
            participant_address,
            admin_signer.pubkey(),
        )],
        Some(&config.default_signer.pubkey()),
    );

    send_and_confirm_message(
        rpc_client,
        message,
        [admin_signer.deref(), config.default_signer.deref()],
        None,
    )
}

fn process_admin_reject(
    config: &Config,
    rpc_client: &RpcClient,
    admin_signer: Box<dyn Signer>,
    participant_address: Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let participants = get_participants(rpc_client)?;
    let participant = participants
        .get(&participant_address)
        .ok_or_else(|| format!("Participant {} does not exist", participant_address))?;

    print_participant(&participant);
    println!("Rejecting...");

    let message = Message::new(
        &[registry_program::instruction::reject(
            participant_address,
            admin_signer.pubkey(),
        )],
        Some(&config.default_signer.pubkey()),
    );

    send_and_confirm_message(
        rpc_client,
        message,
        [admin_signer.deref(), config.default_signer.deref()],
        None,
    )
}

fn process_admin_import(
    config: &Config,
    rpc_client: &RpcClient,
    admin_signer: Box<dyn Signer>,
    mainnet_identity: Pubkey,
    testnet_identity: Pubkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let participants = get_participants_with_identity(
        rpc_client,
        [mainnet_identity, testnet_identity]
            .iter()
            .collect::<HashSet<_>>(),
    )?;

    if !participants.is_empty() {
        return Err("A registration already exists with the provided identity".into());
    }

    let rent = rpc_client.get_minimum_balance_for_rent_exemption(Participant::get_packed_len())?;
    let participant: Box<dyn Signer> = Box::new(Keypair::new());

    let message = Message::new(
        &[
            system_instruction::create_account(
                &config.default_signer.pubkey(),
                &participant.pubkey(),
                rent,
                Participant::get_packed_len() as u64,
                &registry_program::id(),
            ),
            registry_program::instruction::rewrite(
                participant.pubkey(),
                admin_signer.pubkey(),
                Participant {
                    state: ParticipantState::Approved,
                    testnet_identity,
                    mainnet_identity,
                },
            ),
        ],
        Some(&config.default_signer.pubkey()),
    );

    send_and_confirm_message(
        rpc_client,
        message,
        [
            participant.deref(),
            admin_signer.deref(),
            config.default_signer.deref(),
        ],
        Some(rent),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::VersionlessSubcommands)
        .setting(AppSettings::InferSubcommands)
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(&config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("keypair")
                .long("keypair")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .global(true)
                .help("Filepath or URL to a keypair [default: client keypair]"),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .takes_value(false)
                .global(true)
                .help("Show additional information"),
        )
        .arg(
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .global(true)
                .validator(is_url)
                .help("JSON RPC URL for the cluster [default: value from configuration file]"),
        )
        .subcommand(
            SubCommand::with_name("apply")
                .about("Begin a new participant registration")
                .arg(
                    Arg::with_name("mainnet")
                        .long("mainnet")
                        .validator(is_valid_signer)
                        .value_name("ADDRESS")
                        .takes_value(true)
                        .required(true)
                        .help("Mainnet validator identity"),
                )
                .arg(
                    Arg::with_name("testnet")
                        .long("testnet")
                        .validator(is_valid_signer)
                        .value_name("ADDRESS")
                        .takes_value(true)
                        .required(true)
                        .help("Testnet validator identity"),
                )
                .arg(
                    Arg::with_name("confirm")
                        .long("confirm")
                        .help("Add the --confirm flag when you're ready to continue"),
                ),
        )
        .subcommand(
            SubCommand::with_name("status")
                .about("Display registration status")
                .arg(
                    Arg::with_name("identity")
                        .validator(is_valid_pubkey)
                        .value_name("ADDRESS")
                        .takes_value(true)
                        .index(1)
                        .required(true)
                        .help("Testnet or Mainnet validator identity"),
                ),
        )
        .subcommand(
            SubCommand::with_name("withdraw")
                .about("Withdraw your registration")
                .arg(
                    Arg::with_name("identity")
                        .validator(is_valid_pubkey)
                        .value_name("ADDRESS")
                        .takes_value(true)
                        .index(1)
                        .required(true)
                        .help("Testnet or Mainnet validator identity"),
                )
                .arg(
                    Arg::with_name("confirm")
                        .long("confirm")
                        .help("Add the --confirm flag to continue when you're ready to continue"),
                ),
        )
        .subcommand(
            SubCommand::with_name("list")
                .about("List registrations")
                .arg(
                    Arg::with_name("state")
                        .long("state")
                        .value_name("STATE")
                        .possible_values(&["all", "pending", "approved", "rejected"])
                        .default_value("all")
                        .help("Restrict the list to registrations in the specified state"),
                ),
        )
        .subcommand(
            SubCommand::with_name("admin")
                .about("Administration commands")
                .setting(AppSettings::SubcommandRequiredElseHelp)
                .setting(AppSettings::InferSubcommands)
                .arg(
                    Arg::with_name("authority")
                        .long("authority")
                        .validator(is_valid_signer)
                        .value_name("KEYPAIR")
                        .help("Administration authority"),
                )
                .subcommand(
                    SubCommand::with_name("approve")
                        .about("Approve a participant")
                        .arg(
                            Arg::with_name("participant")
                                .validator(is_valid_pubkey)
                                .value_name("ADDRESS")
                                .takes_value(true)
                                .index(1)
                                .required(true)
                                .help("Participant address"),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("reject")
                        .about("Reject a participant")
                        .arg(
                            Arg::with_name("participant")
                                .validator(is_valid_pubkey)
                                .value_name("ADDRESS")
                                .takes_value(true)
                                .index(1)
                                .required(true)
                                .help("Participant address"),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("import")
                        .about("Import an existing participant")
                        .arg(
                            Arg::with_name("testnet")
                                .long("testnet")
                                .validator(is_valid_pubkey)
                                .value_name("ADDRESS")
                                .takes_value(true)
                                .required(true)
                                .help("Testnet validator identity"),
                        )
                        .arg(
                            Arg::with_name("mainnet")
                                .long("mainnet")
                                .validator(is_valid_pubkey)
                                .value_name("ADDRESS")
                                .takes_value(true)
                                .required(true)
                                .help("Mainnet validator identity"),
                        ),
                ),
        )
        .get_matches();

    let (sub_command, sub_matches) = app_matches.subcommand();
    let matches = sub_matches.unwrap();
    let mut wallet_manager: Option<Arc<RemoteWalletManager>> = None;

    let config = {
        let cli_config = if let Some(config_file) = matches.value_of("config_file") {
            solana_cli_config::Config::load(config_file).unwrap_or_default()
        } else {
            solana_cli_config::Config::default()
        };

        let default_signer = DefaultSigner {
            path: matches
                .value_of(&"keypair")
                .map(|s| s.to_string())
                .unwrap_or_else(|| cli_config.keypair_path.clone()),
            arg_name: "keypair".to_string(),
        };

        Config {
            json_rpc_url: matches
                .value_of("json_rpc_url")
                .unwrap_or(&cli_config.json_rpc_url)
                .to_string(),
            default_signer: default_signer
                .signer_from_path(&matches, &mut wallet_manager)
                .unwrap_or_else(|err| {
                    eprintln!("error: {}", err);
                    exit(1);
                }),
            verbose: matches.is_present("verbose"),
        }
    };
    solana_logger::setup_with_default("solana=info");

    if config.verbose {
        println!("JSON RPC URL: {}", config.json_rpc_url);
    }
    let rpc_client =
        RpcClient::new_with_commitment(config.json_rpc_url.clone(), CommitmentConfig::confirmed());

    match (sub_command, sub_matches) {
        ("apply", Some(arg_matches)) => {
            let confirm = arg_matches.is_present("confirm");
            let mainnet_identity_signer =
                match signer_of(arg_matches, "mainnet", &mut wallet_manager) {
                    Err(err) => {
                        eprintln!("Failed to parse mainnet identity: {}", err);
                        exit(1);
                    }
                    Ok((Some(signer), _)) => signer,
                    _ => unreachable!(),
                };
            let testnet_identity_signer =
                match signer_of(arg_matches, "testnet", &mut wallet_manager) {
                    Err(err) => {
                        eprintln!("Failed to parse testnet identity: {}", err);
                        exit(1);
                    }
                    Ok((Some(signer), _)) => signer,
                    _ => unreachable!(),
                };

            process_apply(
                &config,
                &rpc_client,
                mainnet_identity_signer,
                testnet_identity_signer,
                confirm,
            )?;
        }
        ("status", Some(arg_matches)) => {
            let identity = pubkey_of(arg_matches, "identity").unwrap();
            process_status(&config, &rpc_client, identity)?;
        }
        ("withdraw", Some(arg_matches)) => {
            let confirm = arg_matches.is_present("confirm");
            let identity_signer = match signer_of(arg_matches, "identity", &mut wallet_manager) {
                Err(err) => {
                    eprintln!("Failed to parse identity: {}", err);
                    exit(1);
                }
                Ok((Some(signer), _)) => signer,
                _ => unreachable!(),
            };

            process_withdraw(&config, &rpc_client, identity_signer, confirm)?;
        }
        ("list", Some(arg_matches)) => {
            let state = match value_t_or_exit!(arg_matches, "state", String).as_str() {
                "all" => None,
                "pending" => Some(ParticipantState::Pending),
                "rejected" => Some(ParticipantState::Rejected),
                "approved" => Some(ParticipantState::Approved),
                _ => unreachable!(),
            };

            process_list(&config, &rpc_client, state)?;
        }
        ("admin", Some(admin_matches)) => {
            let admin_signer = match signer_of(admin_matches, "authority", &mut wallet_manager) {
                Err(err) => {
                    eprintln!("Failed to parse admin authority: {}", err);
                    exit(1);
                }
                Ok((Some(signer), _)) => signer,
                _ => unreachable!(),
            };

            if admin_signer.pubkey() != registry_program::admin::id() {
                eprintln!("Invalid admin authority");
                exit(1);
            }

            match admin_matches.subcommand() {
                ("approve", Some(arg_matches)) => {
                    let participant = pubkey_of(arg_matches, "participant").unwrap();
                    process_admin_approve(&config, &rpc_client, admin_signer, participant)?;
                }
                ("reject", Some(arg_matches)) => {
                    let participant = pubkey_of(arg_matches, "participant").unwrap();
                    process_admin_reject(&config, &rpc_client, admin_signer, participant)?;
                }
                ("import", Some(arg_matches)) => {
                    let testnet_identity = pubkey_of(arg_matches, "testnet").unwrap();
                    let mainnet_identity = pubkey_of(arg_matches, "mainnet").unwrap();
                    process_admin_import(
                        &config,
                        &rpc_client,
                        admin_signer,
                        mainnet_identity,
                        testnet_identity,
                    )?;
                }
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    };

    Ok(())
}
