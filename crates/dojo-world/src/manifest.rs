use std::collections::HashMap;
use std::fs;
use std::path::Path;

use ::serde::{Deserialize, Serialize};
use cainome::cairo_serde::{ContractAddress, Error as CainomeError};
use cairo_lang_starknet::abi;
use serde_with::serde_as;
use smol_str::SmolStr;
use starknet::core::serde::unsigned_field_element::UfeHex;
use starknet::core::types::{
    BlockId, BlockTag, EmittedEvent, EventFilter, FieldElement, FunctionCall, StarknetError,
};
use starknet::core::utils::{
    parse_cairo_short_string, starknet_keccak, CairoShortStringToFeltError,
    ParseCairoShortStringError,
};
use starknet::macros::selector;
use starknet::providers::{Provider, ProviderError};
use thiserror::Error;

use crate::contracts::model::ModelError;
use crate::contracts::world::WorldEvent;
use crate::contracts::WorldContractReader;

#[cfg(test)]
#[path = "manifest_test.rs"]
mod test;

pub const WORLD_CONTRACT_NAME: &str = "dojo::world::world";
pub const BASE_CONTRACT_NAME: &str = "dojo::base::base";
pub const RESOURCE_METADATA_CONTRACT_NAME: &str = "dojo::resource_metadata::resource_metadata";
pub const RESOURCE_METADATA_MODEL_NAME: &str = "0x5265736f757263654d65746164617461";

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error("Remote World not found.")]
    RemoteWorldNotFound,
    #[error("Entry point name contains non-ASCII characters.")]
    InvalidEntryPointError,
    #[error(transparent)]
    CairoShortStringToFelt(#[from] CairoShortStringToFeltError),
    #[error(transparent)]
    ParseCairoShortString(#[from] ParseCairoShortStringError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    ContractRead(#[from] CainomeError),
    #[error(transparent)]
    Model(#[from] ModelError),
}

/// Represents a model member.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Member {
    /// Name of the member.
    pub name: String,
    /// Type of the member.
    #[serde(rename = "type")]
    pub ty: String,
    pub key: bool,
}

impl From<dojo_types::schema::Member> for Member {
    fn from(m: dojo_types::schema::Member) -> Self {
        Self { name: m.name, ty: m.ty.name(), key: m.key }
    }
}

/// Represents a declaration of a model.
#[serde_as]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Model {
    pub name: String,
    pub members: Vec<Member>,
    #[serde_as(as = "UfeHex")]
    pub class_hash: FieldElement,
    pub abi: Option<abi::Contract>,
}

/// System input ABI.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Input {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
}

/// System Output ABI.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Output {
    #[serde(rename = "type")]
    pub ty: String,
}

#[serde_as]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct ComputedValueEntrypoint {
    // Name of the contract containing the entrypoint
    pub contract: SmolStr,
    // Name of entrypoint to get computed value
    pub entrypoint: SmolStr,
    // Component to compute for
    pub model: Option<String>,
}

#[serde_as]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Contract {
    pub name: SmolStr,
    #[serde_as(as = "Option<UfeHex>")]
    pub address: Option<FieldElement>,
    #[serde_as(as = "UfeHex")]
    pub class_hash: FieldElement,
    pub abi: Option<abi::Contract>,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub computed: Vec<ComputedValueEntrypoint>,
}

#[serde_as]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Class {
    pub name: SmolStr,
    #[serde_as(as = "UfeHex")]
    pub class_hash: FieldElement,
    pub abi: Option<abi::Contract>,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub world: Contract,
    pub base: Class,
    pub resource_metadata: Contract,
    pub contracts: Vec<Contract>,
    pub models: Vec<Model>,
}

impl Manifest {
    /// Load the manifest from a file at the given path.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let file = fs::File::open(path)?;
        Ok(Self::try_from(file)?)
    }

    /// Writes the manifest into a file at the given path. Will return error if the file doesn't
    /// exist.
    pub fn write_to_path(self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let fd = fs::File::options().write(true).open(path)?;
        Ok(serde_json::to_writer_pretty(fd, &self)?)
    }

    /// Construct a manifest of a remote World.
    ///
    /// # Arguments
    /// * `provider` - A Starknet RPC provider.
    /// * `world_address` - The address of the remote World contract.
    pub async fn load_from_remote<P>(
        provider: P,
        world_address: FieldElement,
    ) -> Result<Self, ManifestError>
    where
        P: Provider + Send + Sync,
    {
        const BLOCK_ID: BlockId = BlockId::Tag(BlockTag::Pending);

        let world_class_hash =
            provider.get_class_hash_at(BLOCK_ID, world_address).await.map_err(|err| match err {
                ProviderError::StarknetError(StarknetError::ContractNotFound) => {
                    ManifestError::RemoteWorldNotFound
                }
                err => err.into(),
            })?;

        let world = WorldContractReader::new(world_address, provider);

        let base_class_hash = world.base().block_id(BLOCK_ID).call().await?;

        let (resource_metadata_class_hash, resource_metadata_address) = world
            .model(&FieldElement::from_hex_be(RESOURCE_METADATA_MODEL_NAME).unwrap())
            .block_id(BLOCK_ID)
            .call()
            .await?;

        let resource_metadata_address =
            if resource_metadata_address == ContractAddress(FieldElement::ZERO) {
                None
            } else {
                Some(resource_metadata_address.into())
            };

        let (models, contracts) =
            get_remote_models_and_contracts(world_address, &world.provider()).await?;

        Ok(Manifest {
            models,
            contracts,
            world: Contract {
                name: WORLD_CONTRACT_NAME.into(),
                class_hash: world_class_hash,
                address: Some(world_address),
                ..Default::default()
            },
            resource_metadata: Contract {
                name: RESOURCE_METADATA_CONTRACT_NAME.into(),
                class_hash: resource_metadata_class_hash.into(),
                address: resource_metadata_address,
                ..Default::default()
            },
            base: Class {
                name: BASE_CONTRACT_NAME.into(),
                class_hash: base_class_hash.into(),
                ..Default::default()
            },
        })
    }
}

impl TryFrom<std::fs::File> for Manifest {
    type Error = serde_json::Error;
    fn try_from(file: std::fs::File) -> Result<Self, Self::Error> {
        serde_json::from_reader(std::io::BufReader::new(file))
    }
}

impl TryFrom<&std::fs::File> for Manifest {
    type Error = serde_json::Error;
    fn try_from(file: &std::fs::File) -> Result<Self, Self::Error> {
        serde_json::from_reader(std::io::BufReader::new(file))
    }
}

async fn get_remote_models_and_contracts<P: Provider>(
    world: FieldElement,
    provider: P,
) -> Result<(Vec<Model>, Vec<Contract>), ManifestError>
where
    P: Provider + Send + Sync,
{
    let registered_models_event_name = starknet_keccak("ModelRegistered".as_bytes());
    let contract_deployed_event_name = starknet_keccak("ContractDeployed".as_bytes());
    let contract_upgraded_event_name = starknet_keccak("ContractUpgraded".as_bytes());

    let events = get_events(
        &provider,
        world,
        vec![vec![
            registered_models_event_name,
            contract_deployed_event_name,
            contract_upgraded_event_name,
        ]],
    )
    .await?;

    let mut registered_models_events = vec![];
    let mut contract_deployed_events = vec![];
    let mut contract_upgraded_events = vec![];

    for event in events {
        match event.keys.first() {
            Some(event_name) if *event_name == registered_models_event_name => {
                registered_models_events.push(event)
            }
            Some(event_name) if *event_name == contract_deployed_event_name => {
                contract_deployed_events.push(event)
            }
            Some(event_name) if *event_name == contract_upgraded_event_name => {
                contract_upgraded_events.push(event)
            }
            _ => {}
        }
    }

    let models = parse_models_events(registered_models_events);
    let mut contracts = parse_contracts_events(contract_deployed_events, contract_upgraded_events);

    // fetch contracts name
    for contract in &mut contracts {
        let name = match provider
            .call(
                FunctionCall {
                    calldata: vec![],
                    entry_point_selector: selector!("dojo_resource"),
                    contract_address: contract.address.expect("qed; missing address"),
                },
                BlockId::Tag(BlockTag::Latest),
            )
            .await
        {
            Ok(res) => parse_cairo_short_string(&res[0])?.into(),

            Err(ProviderError::StarknetError(StarknetError::ContractError(_))) => SmolStr::from(""),

            Err(err) => return Err(err.into()),
        };

        contract.name = name;
    }

    Ok((models, contracts))
}

async fn get_events<P: Provider>(
    provider: P,
    world: FieldElement,
    keys: Vec<Vec<FieldElement>>,
) -> Result<Vec<EmittedEvent>, ProviderError> {
    const DEFAULT_CHUNK_SIZE: u64 = 100;

    let mut events: Vec<EmittedEvent> = vec![];
    let mut continuation_token = None;

    let filter =
        EventFilter { to_block: None, from_block: None, address: Some(world), keys: Some(keys) };

    loop {
        let res =
            provider.get_events(filter.clone(), continuation_token, DEFAULT_CHUNK_SIZE).await?;

        continuation_token = res.continuation_token;
        events.extend(res.events);

        if continuation_token.is_none() {
            break;
        }
    }

    Ok(events)
}

fn parse_contracts_events(
    deployed: Vec<EmittedEvent>,
    upgraded: Vec<EmittedEvent>,
) -> Vec<Contract> {
    fn retain_only_latest_upgrade_events(
        events: Vec<EmittedEvent>,
    ) -> HashMap<FieldElement, FieldElement> {
        // addr -> (block_num, class_hash)
        let mut upgrades: HashMap<FieldElement, (u64, FieldElement)> = HashMap::new();

        events.into_iter().for_each(|event| {
            let mut data = event.data.into_iter();

            let block_num = event.block_number;
            let class_hash = data.next().expect("qed; missing class hash");
            let address = data.next().expect("qed; missing address");

            // Events that do not have a block number are ignored because we are unable to evaluate
            // whether the events happened before or after the latest event that has been processed.
            if let Some(num) = block_num {
                upgrades
                    .entry(address)
                    .and_modify(|(current_block, current_class_hash)| {
                        if *current_block < num {
                            *current_block = num;
                            *current_class_hash = class_hash;
                        }
                    })
                    .or_insert((num, class_hash));
            }
        });

        upgrades.into_iter().map(|(addr, (_, class_hash))| (addr, class_hash)).collect()
    }

    let upgradeds = retain_only_latest_upgrade_events(upgraded);

    deployed
        .into_iter()
        .map(|event| {
            let mut data = event.data.into_iter();

            let _ = data.next().expect("salt is missing from event");
            let mut class_hash = data.next().expect("class hash is missing from event");
            let address = data.next().expect("addresss is missing from event");

            if let Some(upgrade) = upgradeds.get(&address) {
                class_hash = *upgrade;
            }

            Contract { address: Some(address), class_hash, ..Default::default() }
        })
        .collect()
}

fn parse_models_events(events: Vec<EmittedEvent>) -> Vec<Model> {
    let mut models: HashMap<String, FieldElement> = HashMap::with_capacity(events.len());

    for e in events {
        let model_event = if let WorldEvent::ModelRegistered(m) =
            e.try_into().expect("ModelRegistered event is expected to be parseable")
        {
            m
        } else {
            panic!("ModelRegistered expected");
        };

        let model_name = parse_cairo_short_string(&model_event.name).unwrap();

        if let Some(current_class_hash) = models.get_mut(&model_name) {
            if current_class_hash == &model_event.prev_class_hash.into() {
                *current_class_hash = model_event.class_hash.into();
            }
        } else {
            models.insert(model_name, model_event.class_hash.into());
        }
    }

    // TODO: include address of the model in the manifest.
    models
        .into_iter()
        .map(|(name, class_hash)| Model { name, class_hash, ..Default::default() })
        .collect()
}
