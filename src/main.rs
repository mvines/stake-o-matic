mod confirmed_block_cache;
mod data_center_info;
mod generic_stake_pool;
mod legacy_stake_pool;
mod rpc_client_utils;
mod stake_pool;
mod validator_list;
mod validators_app;

use {
    crate::generic_stake_pool::*,
    clap::{
        crate_description, crate_name, value_t, value_t_or_exit, App, AppSettings, Arg, ArgMatches,
        SubCommand,
    },
    confirmed_block_cache::ConfirmedBlockCache,
    log::*,
    solana_clap_utils::{
        input_parsers::{keypair_of, pubkey_of},
        input_validators::{
            is_amount, is_keypair, is_pubkey_or_keypair, is_url, is_valid_percentage,
        },
    },
    solana_client::{
        rpc_client::RpcClient,
        rpc_response::{RpcVoteAccountInfo, RpcVoteAccountStatus},
    },
    solana_notifier::Notifier,
    solana_sdk::{
        clock::{Epoch, Slot},
        native_token::*,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    },
    std::{
        collections::{HashMap, HashSet},
        error,
        fs::File,
        path::PathBuf,
        process,
        str::FromStr,
    },
    thiserror::Error,
};

enum InfrastructureConcentrationAffectKind {
    Destake(String),
    Warn(String),
}

#[derive(Debug)]
enum InfrastructureConcentrationAffects {
    WarnAll,
    DestakeListed(HashSet<Pubkey>),
    DestakeAll,
}

impl InfrastructureConcentrationAffects {
    fn destake_memo(validator_id: &Pubkey, concentration: f64, config: &Config) -> String {
        format!(
            "🏟️ `{}` infrastructure concentration {:.1}% is too high. Max concentration is {:.0}%. Removed stake",
            validator_id,
            concentration,
            config.max_infrastructure_concentration,
        )
    }
    fn warning_memo(validator_id: &Pubkey, concentration: f64, config: &Config) -> String {
        format!(
            "🗺  `{}` infrastructure concentration {:.1}% is too high. Max concentration is {:.0}%. No stake removed. Consider finding a new data center",
            validator_id,
            concentration,
            config.max_infrastructure_concentration,
        )
    }
    pub fn memo(
        &self,
        validator_id: &Pubkey,
        concentration: f64,
        config: &Config,
    ) -> InfrastructureConcentrationAffectKind {
        match self {
            Self::DestakeAll => InfrastructureConcentrationAffectKind::Destake(Self::destake_memo(
                validator_id,
                concentration,
                config,
            )),
            Self::WarnAll => InfrastructureConcentrationAffectKind::Warn(Self::warning_memo(
                validator_id,
                concentration,
                config,
            )),
            Self::DestakeListed(ref list) => {
                if list.contains(validator_id) {
                    InfrastructureConcentrationAffectKind::Destake(Self::destake_memo(
                        validator_id,
                        concentration,
                        config,
                    ))
                } else {
                    InfrastructureConcentrationAffectKind::Warn(Self::warning_memo(
                        validator_id,
                        concentration,
                        config,
                    ))
                }
            }
        }
    }
}

#[derive(Debug, Error)]
#[error("cannot convert to InfrastructureConcentrationAffects: {0}")]
struct InfrastructureConcentrationAffectsFromStrError(String);

impl FromStr for InfrastructureConcentrationAffects {
    type Err = InfrastructureConcentrationAffectsFromStrError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "warn" => Ok(Self::WarnAll),
            "destake" => Ok(Self::DestakeAll),
            _ => {
                let file = File::open(s)
                    .map_err(|_| InfrastructureConcentrationAffectsFromStrError(s.to_string()))?;
                let mut list: Vec<String> = serde_yaml::from_reader(file)
                    .map_err(|_| InfrastructureConcentrationAffectsFromStrError(s.to_string()))?;
                let list = list
                    .drain(..)
                    .filter_map(|ref s| Pubkey::from_str(s).ok())
                    .collect::<HashSet<_>>();
                Ok(Self::DestakeListed(list))
            }
        }
    }
}

fn is_release_version(string: String) -> Result<(), String> {
    if string.starts_with('v') && semver::Version::parse(string.split_at(1).1).is_ok() {
        return Ok(());
    }
    semver::Version::parse(&string)
        .map(|_| ())
        .map_err(|err| format!("{:?}", err))
}

fn release_version_of(matches: &ArgMatches<'_>, name: &str) -> Option<semver::Version> {
    matches
        .value_of(name)
        .map(ToString::to_string)
        .map(|string| {
            if string.starts_with('v') {
                semver::Version::parse(string.split_at(1).1)
            } else {
                semver::Version::parse(&string)
            }
            .expect("semver::Version")
        })
}

#[derive(Debug)]
struct Config {
    json_rpc_url: String,
    cluster: String,
    authorized_staker: Keypair,

    dry_run: bool,

    /// Quality validators produce within this percentage of the cluster average skip rate over
    /// the previous epoch
    quality_block_producer_percentage: usize,

    /// A delinquent validator gets this number of slots of grace (from the current slot) before it
    /// will be fully destaked.  The grace period is intended to account for unexpected bugs that
    /// cause a validator to go down
    delinquent_grace_slot_distance: u64,

    /// Don't ever unstake more than this percentage of the cluster at one time for poor block
    /// production
    max_poor_block_producer_percentage: usize,

    /// Vote accounts with a larger commission than this amount will not be staked.
    max_commission: u8,

    /// If Some(), destake validators with a version less than this version subject to the
    /// `max_old_release_version_percentage` limit
    min_release_version: Option<semver::Version>,

    /// Don't ever unstake more than this percentage of the cluster at one time for running an
    /// older software version
    max_old_release_version_percentage: usize,

    /// Base path of confirmed block cache
    confirmed_block_cache_path: PathBuf,

    /// Vote accounts sharing infrastructure with larger than this amount will not be staked
    max_infrastructure_concentration: f64,

    /// How validators with infrastruction concentration above `max_infrastructure_concentration`
    /// will be affected. Accepted values are:
    /// 1) "warn"       - Stake unaffected. A warning message is notified
    /// 2) "destake"    - Removes all validator stake
    /// 3) PATH_TO_YAML - Reads a list of validator identity pubkeys from the specified YAML file
    ///                   destaking those in the list and warning any others
    infrastructure_concentration_affects: InfrastructureConcentrationAffects,

    /// Use a cluster-average skip rate floor for block-production quality calculations
    use_cluster_average_skip_rate: bool,

    bad_cluster_average_skip_rate: usize,
}

impl Config {
    #[cfg(test)]
    pub fn default_for_test() -> Self {
        Self {
            json_rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            cluster: "mainnet-beta".to_string(),
            authorized_staker: Keypair::new(),
            dry_run: true,
            quality_block_producer_percentage: 15,
            delinquent_grace_slot_distance: 21_600,
            max_poor_block_producer_percentage: 20,
            max_commission: 100,
            min_release_version: None,
            max_old_release_version_percentage: 10,
            confirmed_block_cache_path: default_confirmed_block_cache_path(),
            max_infrastructure_concentration: 100.0,
            infrastructure_concentration_affects: InfrastructureConcentrationAffects::WarnAll,
            use_cluster_average_skip_rate: false,
            bad_cluster_average_skip_rate: 50,
        }
    }
}

fn default_confirmed_block_cache_path() -> PathBuf {
    let home_dir = std::env::var("HOME").unwrap();
    PathBuf::from(home_dir).join(".cache/solana/som/confirmed-block-cache/")
}

fn app_version() -> String {
    // Determine version based on the environment variables set by Github Actions
    let tag = option_env!("GITHUB_REF")
        .and_then(|github_ref| github_ref.strip_prefix("refs/tags/").map(|s| s.to_string()));

    tag.unwrap_or_else(|| match option_env!("GITHUB_SHA") {
        None => "devbuild".to_string(),
        Some(commit) => commit[..8].to_string(),
    })
}

fn get_config() -> (Config, Box<dyn GenericStakePool>) {
    let default_confirmed_block_cache_path = default_confirmed_block_cache_path()
        .to_str()
        .unwrap()
        .to_string();
    let app_version = &*app_version();
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(app_version)
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
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url)
                .help("JSON RPC URL for the cluster")
        )
        .arg(
            Arg::with_name("cluster")
                .long("cluster")
                .value_name("NAME")
                .possible_values(&["mainnet-beta", "testnet"])
                .takes_value(true)
                .help("Name of the cluster to operate on")
        )
        .arg(
            Arg::with_name("confirm")
                .long("confirm")
                .takes_value(false)
                .help("Confirm that the stake adjustments should actually be made")
        )
        .arg(
            Arg::with_name("quality_block_producer_percentage")
                .long("quality-block-producer-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("15")
                .validator(is_valid_percentage)
                .help("Quality validators have a skip rate within this percentage of \
                       the cluster average in the previous epoch.")
        )
        .arg(
            Arg::with_name("bad_cluster_average_skip_rate")
                .long("bad-cluster-average-skip-rate")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("50")
                .validator(is_valid_percentage)
                .help("Threshold to notify for a poor average cluster skip rate.")
        )
        .arg(
            Arg::with_name("max_poor_block_producer_percentage")
                .long("max-poor-block-producer-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("20")
                .validator(is_valid_percentage)
                .help("Do not add or remove bonus stake from any
                       non-delinquent validators if at least this percentage of \
                       all validators are poor block producers")
        )
        .arg(
            Arg::with_name("max_commission")
                .long("max-commission")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("100")
                .validator(is_valid_percentage)
                .help("Vote accounts with a larger commission than this amount will not be staked")
        )
        .arg(
            Arg::with_name("min_release_version")
                .long("min-release-version")
                .value_name("SEMVER")
                .takes_value(true)
                .validator(is_release_version)
                .help("Remove the base and bonus stake from validators with \
                       a release version older than this one")
        )
        .arg(
            Arg::with_name("max_old_release_version_percentage")
                .long("max-old-release-version-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("10")
                .validator(is_valid_percentage)
                .help("Do not remove stake from validators running older \
                       software versions if more than this percentage of \
                       all validators are running an older software version")
        )
        .arg(
            Arg::with_name("confirmed_block_cache_path")
                .long("confirmed-block-cache-path")
                .takes_value(true)
                .value_name("PATH")
                .default_value(&default_confirmed_block_cache_path)
                .help("Base path of confirmed block cache")
        )
        .arg(
            Arg::with_name("max_infrastructure_concentration")
                .long("max-infrastructure-concentration")
                .takes_value(true)
                .value_name("PERCENTAGE")
                .default_value("100")
                .validator(is_valid_percentage)
                .help("Vote accounts sharing infrastructure with larger than this amount will not be staked")
        )
        .arg(
            Arg::with_name("infrastructure_concentration_affects")
                .long("infrastructure-concentration-affects")
                .takes_value(true)
                .value_name("AFFECTS")
                .default_value("warn")
                .validator(|ref s| {
                    InfrastructureConcentrationAffects::from_str(s)
                        .map(|_| ())
                        .map_err(|e| format!("{}", e))
                })
                .help("How validators with infrastruction concentration above \
                       `max_infrastructure_concentration` will be affected. \
                       Accepted values are: \
                       1) warn         - Stake unaffected. A warning message \
                                         is notified \
                       2) destake      - Removes all validator stake \
                       3) PATH_TO_YAML - Reads a list of validator identity \
                                         pubkeys from the specified YAML file \
                                         destaking those in the list and warning \
                                         any others")
        )
        .arg(
            Arg::with_name("use_cluster_average_skip_rate")
                .long("use-cluster-average-skip-rate")
                .help("Use a cluster-average skip rate floor for block-production quality calculations")
        )


        .arg(
            Arg::with_name("authorized_staker")
                .index(1)
                .value_name("KEYPAIR")
                .validator(is_keypair)
                .required(true)
                .takes_value(true)
                .help("Keypair of the authorized staker")
        )
        .subcommand(
            SubCommand::with_name("legacy").about("Use the legacy staking solution")
            .arg(
                Arg::with_name("source_stake_address")
                    .index(1)
                    .value_name("SOURCE_STAKE_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .validator(is_pubkey_or_keypair)
                    .help("The source stake account for splitting individual validator stake accounts from")
            )
            .arg(
                Arg::with_name("baseline_stake_amount")
                    .long("baseline-stake-amount")
                    .value_name("SOL")
                    .takes_value(true)
                    .default_value("5000")
                    .validator(is_amount)
            )
            .arg(
                Arg::with_name("bonus_stake_amount")
                    .long("bonus-stake-amount")
                    .value_name("SOL")
                    .takes_value(true)
                    .default_value("50000")
                    .validator(is_amount)
            )
            .arg(
                Arg::with_name("--validator-list")
                    .long("validator-list")
                    .value_name("FILE")
                    .takes_value(true)
                    .conflicts_with("cluster")
                    .help("File containing an YAML array of validator pubkeys eligible for staking")
            )
        )
        .subcommand(
            SubCommand::with_name("stake-pool").about("Use a stake pool")
            .arg(
                Arg::with_name("pool_address")
                    .index(1)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .validator(is_pubkey_or_keypair)
                    .help("The stake pool address")
            )
            .arg(
                Arg::with_name("baseline_stake_amount")
                    .long("baseline-stake-amount")
                    .value_name("SOL")
                    .takes_value(true)
                    .default_value("5000")
                    .validator(is_amount)
            )
        )
        .get_matches();

    let config = if let Some(config_file) = matches.value_of("config_file") {
        solana_cli_config::Config::load(config_file).unwrap_or_default()
    } else {
        solana_cli_config::Config::default()
    };

    let dry_run = !matches.is_present("confirm");
    let cluster = value_t!(matches, "cluster", String).unwrap_or_else(|_| "unknown".into());
    let quality_block_producer_percentage =
        value_t_or_exit!(matches, "quality_block_producer_percentage", usize);
    let max_commission = value_t_or_exit!(matches, "max_commission", u8);
    let max_poor_block_producer_percentage =
        value_t_or_exit!(matches, "max_poor_block_producer_percentage", usize);
    let max_old_release_version_percentage =
        value_t_or_exit!(matches, "max_old_release_version_percentage", usize);
    let min_release_version = release_version_of(&matches, "min_release_version");

    let json_rpc_url = match cluster.as_str() {
        "mainnet-beta" => value_t!(matches, "json_rpc_url", String)
            .unwrap_or_else(|_| "http://api.mainnet-beta.solana.com".into()),
        "testnet" => value_t!(matches, "json_rpc_url", String)
            .unwrap_or_else(|_| "http://testnet.solana.com".into()),
        "unknown" => value_t!(matches, "json_rpc_url", String)
            .unwrap_or_else(|_| config.json_rpc_url.clone()),
        _ => unreachable!(),
    };

    let confirmed_block_cache_path = matches
        .value_of("confirmed_block_cache_path")
        .map(PathBuf::from)
        .unwrap();

    let bad_cluster_average_skip_rate =
        value_t!(matches, "bad_cluster_average_skip_rate", usize).unwrap_or(50);
    let max_infrastructure_concentration =
        value_t!(matches, "max_infrastructure_concentration", f64).unwrap();
    let infrastructure_concentration_affects = value_t!(
        matches,
        "infrastructure_concentration_affects",
        InfrastructureConcentrationAffects
    )
    .unwrap();
    let use_cluster_average_skip_rate = matches.is_present("use_cluster_average_skip_rate");

    let authorized_staker = keypair_of(&matches, "authorized_staker").unwrap();

    let config = Config {
        json_rpc_url,
        cluster,
        authorized_staker,
        dry_run,
        delinquent_grace_slot_distance: 21600, // ~24 hours worth of slots at 2.5 slots per second
        quality_block_producer_percentage,
        max_commission,
        max_poor_block_producer_percentage,
        min_release_version,
        max_old_release_version_percentage,
        confirmed_block_cache_path,
        max_infrastructure_concentration,
        infrastructure_concentration_affects,
        use_cluster_average_skip_rate,
        bad_cluster_average_skip_rate,
    };

    info!("RPC URL: {}", config.json_rpc_url);

    let stake_pool: Box<dyn GenericStakePool> = match matches.subcommand() {
        ("legacy", Some(matches)) => {
            let source_stake_address = pubkey_of(&matches, "source_stake_address").unwrap();
            let baseline_stake_amount =
                sol_to_lamports(value_t_or_exit!(matches, "baseline_stake_amount", f64));
            let bonus_stake_amount =
                sol_to_lamports(value_t_or_exit!(matches, "bonus_stake_amount", f64));
            let validator_list = match config.cluster.as_str() {
                "mainnet-beta" => validator_list::mainnet_beta_validators(),
                "testnet" => validator_list::testnet_validators(),
                "unknown" => {
                    let validator_list_file =
                        File::open(value_t_or_exit!(matches, "--validator-list", PathBuf))
                            .unwrap_or_else(|err| {
                                error!("Unable to open validator_list: {}", err);
                                process::exit(1);
                            });

                    serde_yaml::from_reader::<_, Vec<String>>(validator_list_file)
                        .unwrap_or_else(|err| {
                            error!("Unable to read validator_list: {}", err);
                            process::exit(1);
                        })
                        .into_iter()
                        .map(|p| {
                            Pubkey::from_str(&p).unwrap_or_else(|err| {
                                error!("Invalid validator_list pubkey '{}': {}", p, err);
                                process::exit(1);
                            })
                        })
                        .collect()
                }
                _ => unreachable!(),
            }
            .into_iter()
            .collect::<HashSet<_>>();
            Box::new(legacy_stake_pool::new(
                baseline_stake_amount,
                bonus_stake_amount,
                source_stake_address,
                validator_list,
            ))
        }
        ("stake-pool", Some(matches)) => {
            let pool_address = pubkey_of(&matches, "pool_address").unwrap();
            let baseline_stake_amount =
                sol_to_lamports(value_t_or_exit!(matches, "baseline_stake_amount", f64));
            Box::new(stake_pool::new(pool_address, baseline_stake_amount))
        }
        _ => unreachable!(),
    };

    (config, stake_pool)
}

type BoxResult<T> = Result<T, Box<dyn error::Error>>;

///                    quality          poor             cluster_skip_rate, too_many_poor_block_producers
type ClassifyResult = (HashSet<Pubkey>, HashSet<Pubkey>, usize, bool);

fn classify_producers(
    first_slot_in_epoch: Slot,
    confirmed_blocks: HashSet<u64>,
    leader_schedule: HashMap<String, Vec<usize>>,
    config: &Config,
) -> BoxResult<ClassifyResult> {
    let mut poor_block_producers = HashSet::new();
    let mut quality_block_producers = HashSet::new();
    let mut blocks_and_slots = HashMap::new();

    let mut total_blocks = 0;
    let mut total_slots = 0;
    for (validator_identity, relative_slots) in leader_schedule {
        let mut validator_blocks = 0;
        let mut validator_slots = 0;
        for relative_slot in relative_slots {
            let slot = first_slot_in_epoch + relative_slot as Slot;
            total_slots += 1;
            validator_slots += 1;
            if confirmed_blocks.contains(&slot) {
                total_blocks += 1;
                validator_blocks += 1;
            }
        }
        if validator_slots > 0 {
            let validator_identity = Pubkey::from_str(&validator_identity)?;
            let e = blocks_and_slots.entry(validator_identity).or_insert((0, 0));
            e.0 += validator_blocks;
            e.1 += validator_slots;
        }
    }
    let cluster_average_skip_rate = 100 - total_blocks * 100 / total_slots;
    for (validator_identity, (blocks, slots)) in blocks_and_slots {
        let skip_rate: usize = 100 - (blocks * 100 / slots);
        let skip_rate_floor = if config.use_cluster_average_skip_rate {
            cluster_average_skip_rate
        } else {
            0
        };
        if skip_rate.saturating_sub(config.quality_block_producer_percentage) > skip_rate_floor {
            poor_block_producers.insert(validator_identity);
        } else {
            quality_block_producers.insert(validator_identity);
        }
        trace!(
            "Validator {} produced {} blocks in {} slots skip_rate: {}",
            validator_identity,
            blocks,
            slots,
            skip_rate,
        );
    }

    let poor_block_producer_percentage = poor_block_producers.len() * 100
        / (quality_block_producers.len() + poor_block_producers.len());
    let too_many_poor_block_producers =
        poor_block_producer_percentage > config.max_poor_block_producer_percentage;

    info!("cluster_average_skip_rate: {}", cluster_average_skip_rate);
    info!("quality_block_producers: {}", quality_block_producers.len());
    trace!("quality_block_producers: {:?}", quality_block_producers);
    info!("poor_block_producers: {}", poor_block_producers.len());
    trace!("poor_block_producers: {:?}", poor_block_producers);
    info!(
        "poor_block_producer_percentage: {}% (too many poor producers={})",
        poor_block_producer_percentage, too_many_poor_block_producers,
    );

    Ok((
        quality_block_producers,
        poor_block_producers,
        cluster_average_skip_rate,
        too_many_poor_block_producers,
    ))
}

/// Split validators into quality/poor lists based on their block production over the given `epoch`
fn classify_block_producers(
    rpc_client: &RpcClient,
    config: &Config,
    epoch: Epoch,
) -> BoxResult<ClassifyResult> {
    let epoch_schedule = rpc_client.get_epoch_schedule()?;
    let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);
    let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);

    let first_available_block = rpc_client.get_first_available_block()?;
    if first_available_block >= first_slot_in_epoch {
        return Err(format!(
            "First available block is newer than the start of epoch {}: {} > {}",
            epoch, first_available_block, first_slot_in_epoch,
        )
        .into());
    }

    let leader_schedule = rpc_client
        .get_leader_schedule(Some(first_slot_in_epoch))?
        .unwrap();

    let cache_path = config.confirmed_block_cache_path.join(&config.cluster);
    let cbc = ConfirmedBlockCache::open(cache_path, &config.json_rpc_url).unwrap();
    let confirmed_blocks = cbc
        .query(first_slot_in_epoch, last_slot_in_epoch)?
        .into_iter()
        .collect::<HashSet<_>>();

    classify_producers(
        first_slot_in_epoch,
        confirmed_blocks,
        leader_schedule,
        config,
    )
}

fn main() -> Result<(), Box<dyn error::Error>> {
    solana_logger::setup_with_default("solana=info");
    let (config, mut stake_pool) = get_config();

    let notifier = if config.dry_run {
        Notifier::new("DRYRUN")
    } else {
        Notifier::default()
    };

    if !config.dry_run && notifier.is_empty() {
        return Err("A notifier must be active with --confirm".into());
    }

    let rpc_client = RpcClient::new(config.json_rpc_url.clone());

    // Sanity check that the RPC endpoint is healthy before performing too much work
    rpc_client
        .get_health()
        .map_err(|err| format!("RPC endpoint is unhealthy: {:?}", err))?;

    let epoch_info = rpc_client.get_epoch_info()?;
    info!("Epoch info: {:?}", epoch_info);

    let vote_account_info = {
        let RpcVoteAccountStatus {
            current,
            delinquent,
        } = rpc_client.get_vote_accounts()?;

        let mut latest_vote_account_info = HashMap::<String, _>::new();

        for vote_account_info in current.into_iter().chain(delinquent.into_iter()) {
            let entry = latest_vote_account_info
                .entry(vote_account_info.node_pubkey.clone())
                .or_insert_with(|| vote_account_info.clone());

            // If the validator has multiple staked vote accounts then select the vote account that
            // voted most recently
            if entry.last_vote < vote_account_info.last_vote {
                *entry = vote_account_info.clone();
            }
        }

        latest_vote_account_info
            .values()
            .cloned()
            .collect::<Vec<_>>()
    };

    let init_transactions = stake_pool.init(
        &rpc_client,
        config.authorized_staker.pubkey(),
        vote_account_info
            .iter()
            .map(|vai| ValidatorAddressPair {
                identity: Pubkey::from_str(&vai.node_pubkey).unwrap(),
                vote_address: Pubkey::from_str(&vai.vote_pubkey).unwrap(),
            })
            .collect::<Vec<_>>(),
        &epoch_info,
    )?;
    if !rpc_client_utils::send_and_confirm_transactions(
        &rpc_client,
        config.dry_run,
        init_transactions,
        &config.authorized_staker,
        &mut vec![],
    )? {
        return Err("Failed to initialize stake pool. Unable to continue".into());
    }

    let cluster_nodes_with_old_version: HashSet<String> = match config.min_release_version {
        Some(ref min_release_version) => rpc_client
            .get_cluster_nodes()?
            .into_iter()
            .filter_map(|rpc_contact_info| {
                if let Ok(pubkey) = Pubkey::from_str(&rpc_contact_info.pubkey) {
                    if stake_pool.is_enrolled(&pubkey) {
                        if let Some(ref version) = rpc_contact_info.version {
                            if let Ok(semver) = semver::Version::parse(version) {
                                if semver < *min_release_version {
                                    return Some(rpc_contact_info.pubkey);
                                }
                            }
                        }
                    }
                }
                None
            })
            .collect(),
        None => HashSet::default(),
    };

    if let Some(ref min_release_version) = config.min_release_version {
        info!(
            "Validators running a release older than {}: {:?}",
            min_release_version, cluster_nodes_with_old_version,
        );
    }

    let last_epoch = epoch_info.epoch - 1;
    let mut notifications = vec![];

    let (
        quality_block_producers,
        poor_block_producers,
        cluster_average_skip_rate,
        too_many_poor_block_producers,
    ) = classify_block_producers(&rpc_client, &config, last_epoch)?;

    if cluster_average_skip_rate > config.bad_cluster_average_skip_rate {
        notifications.push(format!(
            "Cluster average skip rate: {} is above threshold: {}",
            cluster_average_skip_rate, config.bad_cluster_average_skip_rate
        ));
    }

    if too_many_poor_block_producers {
        notifications.push(format!(
            "Over {}% of validators classified as poor block producers in epoch {}",
            config.max_poor_block_producer_percentage, last_epoch,
        ));
    }

    let too_many_old_validators = cluster_nodes_with_old_version.len()
        > (poor_block_producers.len() + quality_block_producers.len())
            * config.max_old_release_version_percentage
            / 100;

    if too_many_old_validators {
        notifications.push(format!(
            "Over {}% of validators classified as running an older release",
            config.max_old_release_version_percentage
        ));
    }

    let infrastructure_concentration = data_center_info::get()
        .map_err(|e| {
            warn!("infrastructure concentration skipped: {}", e);
            e
        })
        .unwrap_or_default()
        .drain(..)
        .filter_map(|dci| {
            if dci.stake_percent > config.max_infrastructure_concentration {
                Some((dci.validators, dci.stake_percent))
            } else {
                None
            }
        })
        .flat_map(|(v, sp)| v.into_iter().map(move |v| (v, sp)))
        .collect::<HashMap<_, _>>();

    let mut desired_validator_stake = vec![];
    for RpcVoteAccountInfo {
        commission,
        node_pubkey: node_pubkey_str,
        root_slot,
        vote_pubkey,
        ..
    } in &vote_account_info
    {
        let node_pubkey = Pubkey::from_str(&node_pubkey_str).unwrap();
        if !stake_pool.is_enrolled(&node_pubkey) {
            continue;
        }

        let infrastructure_concentration_destake_memo = infrastructure_concentration
            .get(&node_pubkey)
            .map(|concentration| {
                config.infrastructure_concentration_affects.memo(
                    &node_pubkey,
                    *concentration,
                    &config,
                )
            })
            .and_then(|affect| match affect {
                InfrastructureConcentrationAffectKind::Destake(memo) => Some(memo),
                InfrastructureConcentrationAffectKind::Warn(memo) => {
                    notifications.push(memo);
                    None
                }
            });

        let operation = if let Some(memo) = infrastructure_concentration_destake_memo {
            Some((ValidatorStakeState::None, memo))
        } else if *commission > config.max_commission {
            Some((
                ValidatorStakeState::None,
                format!(
                    "⛔ `{}` {}% commission is too high",
                    node_pubkey, commission,
                ),
            ))
        } else if !too_many_old_validators
            && cluster_nodes_with_old_version.contains(node_pubkey_str)
        {
            Some((
                ValidatorStakeState::None,
                format!("🧮 `{}` is running an old software release", node_pubkey),
            ))

        // Destake the validator if it has been delinquent for longer than the grace period
        } else if *root_slot
            < epoch_info
                .absolute_slot
                .saturating_sub(config.delinquent_grace_slot_distance)
        {
            Some((
                ValidatorStakeState::None,
                format!("🏖️ `{}` is delinquent", node_pubkey),
            ))

        // Validator is delinquent but less than the grace period.  Take no action
        } else if *root_slot < epoch_info.absolute_slot.saturating_sub(256) {
            None
        } else if quality_block_producers.contains(&node_pubkey) {
            Some((
                ValidatorStakeState::Bonus,
                format!(
                    "🏅 `{}` was a quality block producer during epoch {}",
                    node_pubkey, last_epoch,
                ),
            ))
        } else if poor_block_producers.contains(&node_pubkey) {
            if too_many_poor_block_producers {
                None
            } else {
                Some((
                    ValidatorStakeState::Baseline,
                    format!(
                        "💔 `{}` was a poor block producer during epoch {}",
                        node_pubkey, last_epoch,
                    ),
                ))
            }
        } else {
            Some((
                ValidatorStakeState::Baseline,
                format!("🥩 `{}` is current", node_pubkey),
            ))
        };

        debug!(
            "\nidentity: {}\n - vote address: {}\n - root slot: {}\n - operation: {:?}",
            node_pubkey, vote_pubkey, root_slot, operation
        );
        if let Some((stake_state, memo)) = operation {
            desired_validator_stake.push(ValidatorStake {
                identity: node_pubkey,
                stake_state,
                memo,
            });
        }
    }

    let transactions = stake_pool.apply(
        &rpc_client,
        config.authorized_staker.pubkey(),
        desired_validator_stake,
    )?;

    let ok = rpc_client_utils::send_and_confirm_transactions(
        &rpc_client,
        config.dry_run,
        transactions,
        &config.authorized_staker,
        &mut notifications,
    )?;

    for notification in notifications {
        warn!("{}", notification);
        notifier.send(&notification);
    }

    if !ok {
        Err("One or more transactions failed to execute".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_quality_producer_with_average_skip_rate() {
        solana_logger::setup();
        let config = Config {
            quality_block_producer_percentage: 10,
            max_poor_block_producer_percentage: 40,
            use_cluster_average_skip_rate: true,
            ..Config::default_for_test()
        };

        let confirmed_blocks: HashSet<Slot> = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 14, 21, 22, 43, 44, 45, 46, 47, 48,
        ]
        .iter()
        .cloned()
        .collect();
        let mut leader_schedule = HashMap::new();
        let l1 = Pubkey::new_unique();
        let l2 = Pubkey::new_unique();
        let l3 = Pubkey::new_unique();
        let l4 = Pubkey::new_unique();
        let l5 = Pubkey::new_unique();
        leader_schedule.insert(l1.to_string(), (0..10).collect());
        leader_schedule.insert(l2.to_string(), (10..20).collect());
        leader_schedule.insert(l3.to_string(), (20..30).collect());
        leader_schedule.insert(l4.to_string(), (30..40).collect());
        leader_schedule.insert(l5.to_string(), (40..50).collect());
        let (quality, poor, _cluster_average, too_many_poor_block_producers) =
            classify_producers(0, confirmed_blocks, leader_schedule, &config).unwrap();
        assert!(quality.contains(&l1));
        assert!(quality.contains(&l5));
        assert!(quality.contains(&l2));
        assert!(poor.contains(&l3));
        assert!(poor.contains(&l4));
        assert!(!too_many_poor_block_producers);
    }

    #[test]
    fn test_quality_producer_when_all_poor() {
        solana_logger::setup();
        let config = Config {
            quality_block_producer_percentage: 10,
            use_cluster_average_skip_rate: false,
            ..Config::default_for_test()
        };

        let confirmed_blocks = HashSet::<Slot>::new();
        let mut leader_schedule = HashMap::new();
        let l1 = Pubkey::new_unique();
        let l2 = Pubkey::new_unique();
        let l3 = Pubkey::new_unique();
        let l4 = Pubkey::new_unique();
        let l5 = Pubkey::new_unique();
        leader_schedule.insert(l1.to_string(), (0..10).collect());
        leader_schedule.insert(l2.to_string(), (10..20).collect());
        leader_schedule.insert(l3.to_string(), (20..30).collect());
        leader_schedule.insert(l4.to_string(), (30..40).collect());
        leader_schedule.insert(l5.to_string(), (40..50).collect());
        let (quality, poor, _cluster_average, too_many_poor_block_producers) =
            classify_producers(0, confirmed_blocks, leader_schedule, &config).unwrap();
        assert!(quality.is_empty());
        assert_eq!(poor.len(), 5);
        assert!(too_many_poor_block_producers);
    }

    #[test]
    fn test_quality_producer_when_all_good() {
        solana_logger::setup();
        let config = Config {
            quality_block_producer_percentage: 10,
            use_cluster_average_skip_rate: false,
            ..Config::default_for_test()
        };

        let confirmed_blocks: HashSet<Slot> = (0..50).collect();
        let mut leader_schedule = HashMap::new();
        let l1 = Pubkey::new_unique();
        let l2 = Pubkey::new_unique();
        let l3 = Pubkey::new_unique();
        let l4 = Pubkey::new_unique();
        let l5 = Pubkey::new_unique();
        leader_schedule.insert(l1.to_string(), (0..10).collect());
        leader_schedule.insert(l2.to_string(), (10..20).collect());
        leader_schedule.insert(l3.to_string(), (20..30).collect());
        leader_schedule.insert(l4.to_string(), (30..40).collect());
        leader_schedule.insert(l5.to_string(), (40..50).collect());
        let (quality, poor, _cluster_average, too_many_poor_block_producers) =
            classify_producers(0, dbg!(confirmed_blocks), leader_schedule, &config).unwrap();
        assert!(poor.is_empty());
        assert_eq!(quality.len(), 5);
        assert!(!too_many_poor_block_producers);
    }
}
