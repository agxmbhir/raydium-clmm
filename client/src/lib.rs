#![allow(dead_code)]

// Re-export all the instruction modules and their functionality
pub mod instructions;

// Re-export commonly used types and functions from main.rs that might be useful
use anchor_client::{ Client, Cluster };
use anchor_lang::prelude::AccountMeta;
use anyhow::{ format_err, Result };
use arrayref::array_ref;
use configparser::ini::Ini;
use solana_account_decoder::{
    parse_token::{ TokenAccountType, UiAccountState },
    UiAccountData,
    UiAccountEncoding,
};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{ RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig },
    rpc_filter::{ Memcmp, RpcFilterType },
    rpc_request::TokenAccountsFilter,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    message::Message,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{ Keypair, Signature, Signer },
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::{ collections::VecDeque, convert::identity, mem::size_of };

use raydium_amm_v3::{
    libraries::{ fixed_point_64, liquidity_math, tick_math },
    states::{ PoolState, TickArrayBitmapExtension, TickArrayState, POOL_TICK_ARRAY_BITMAP_SEED },
};
use spl_associated_token_account::get_associated_token_address;
use spl_token_2022::{
    extension::StateWithExtensions,
    state::Mint,
    state::{ Account, AccountState },
};
use spl_token_client::token::ExtensionInitializationParams;

// Re-export useful types and functions that other crates might need
pub use instructions::utils::*;

#[derive(Clone, Debug, PartialEq)]
pub struct ClientConfig {
    pub http_url: String,
    pub ws_url: String,
    pub payer_path: String,
    pub admin_path: String,
    pub raydium_v3_program: Pubkey,
    pub slippage: f64,
    pub amm_config_key: Pubkey,
    pub mint0: Option<Pubkey>,
    pub mint1: Option<Pubkey>,
    pub pool_id_account: Option<Pubkey>,
    pub tickarray_bitmap_extension: Option<Pubkey>,
    pub amm_config_index: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PoolAccounts {
    pub pool_id: Option<Pubkey>,
    pub pool_config: Option<Pubkey>,
    pub pool_observation: Option<Pubkey>,
    pub pool_protocol_positions: Vec<Pubkey>,
    pub pool_personal_positions: Vec<Pubkey>,
    pub pool_tick_arrays: Vec<Pubkey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionNftTokenInfo {
    pub key: Pubkey,
    pub program: Pubkey,
    pub position: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub decimals: u8,
}

// Utility functions that might be useful for other crates
pub fn load_cfg(client_config: &String) -> Result<ClientConfig> {
    let mut config = Ini::new();
    let _map = config.load(client_config).unwrap();
    let http_url = config.get("Global", "http_url").unwrap();
    if http_url.is_empty() {
        panic!("http_url must not be empty");
    }
    let ws_url = config.get("Global", "ws_url").unwrap();
    if ws_url.is_empty() {
        panic!("ws_url must not be empty");
    }
    let payer_path = config.get("Global", "payer_path").unwrap();
    if payer_path.is_empty() {
        panic!("payer_path must not be empty");
    }
    let admin_path = config.get("Global", "admin_path").unwrap();
    if admin_path.is_empty() {
        panic!("admin_path must not be empty");
    }

    let raydium_v3_program_str = config.get("Global", "raydium_v3_program").unwrap();
    if raydium_v3_program_str.is_empty() {
        panic!("raydium_v3_program must not be empty");
    }
    let raydium_v3_program = Pubkey::from_str(&raydium_v3_program_str).unwrap();
    let slippage = config.getfloat("Global", "slippage").unwrap().unwrap();

    let mut mint0 = None;
    let mint0_str = config.get("Pool", "mint0").unwrap();
    if !mint0_str.is_empty() {
        mint0 = Some(Pubkey::from_str(&mint0_str).unwrap());
    }
    let mut mint1 = None;
    let mint1_str = config.get("Pool", "mint1").unwrap();
    if !mint1_str.is_empty() {
        mint1 = Some(Pubkey::from_str(&mint1_str).unwrap());
    }
    let amm_config_index = config.getuint("Pool", "amm_config_index").unwrap().unwrap() as u16;

    let (amm_config_key, __bump) = Pubkey::find_program_address(
        &[raydium_amm_v3::states::AMM_CONFIG_SEED.as_bytes(), &amm_config_index.to_be_bytes()],
        &raydium_v3_program
    );

    let pool_id_account = if mint0 != None && mint1 != None {
        if mint0.unwrap() > mint1.unwrap() {
            let temp_mint = mint0;
            mint0 = mint1;
            mint1 = temp_mint;
        }
        Some(
            Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::POOL_SEED.as_bytes(),
                    amm_config_key.to_bytes().as_ref(),
                    mint0.unwrap().to_bytes().as_ref(),
                    mint1.unwrap().to_bytes().as_ref(),
                ],
                &raydium_v3_program
            ).0
        )
    } else {
        None
    };
    let tickarray_bitmap_extension = if pool_id_account != None {
        Some(
            Pubkey::find_program_address(
                &[
                    POOL_TICK_ARRAY_BITMAP_SEED.as_bytes(),
                    pool_id_account.unwrap().to_bytes().as_ref(),
                ],
                &raydium_v3_program
            ).0
        )
    } else {
        None
    };

    Ok(ClientConfig {
        http_url,
        ws_url,
        payer_path,
        admin_path,
        raydium_v3_program,
        slippage,
        amm_config_key,
        mint0,
        mint1,
        pool_id_account,
        tickarray_bitmap_extension,
        amm_config_index,
    })
}

pub fn read_keypair_file(s: &str) -> Result<Keypair> {
    solana_sdk::signature
        ::read_keypair_file(s)
        .map_err(|_| format_err!("failed to read keypair from {}", s))
}

pub fn write_keypair_file(keypair: &Keypair, outfile: &str) -> Result<String> {
    solana_sdk::signature
        ::write_keypair_file(keypair, outfile)
        .map_err(|_| format_err!("failed to write keypair to {}", outfile))
}

pub fn path_is_exist(path: &str) -> bool {
    Path::new(path).exists()
}

pub fn load_cur_and_next_five_tick_array(
    rpc_client: &RpcClient,
    pool_config: &ClientConfig,
    pool_state: &PoolState,
    tickarray_bitmap_extension: &TickArrayBitmapExtension,
    zero_for_one: bool
) -> VecDeque<TickArrayState> {
    let (_, mut current_valid_tick_array_start_index) = pool_state
        .get_first_initialized_tick_array(&Some(*tickarray_bitmap_extension), zero_for_one)
        .unwrap();
    let mut tick_array_keys = Vec::new();
    tick_array_keys.push(
        Pubkey::find_program_address(
            &[
                raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                &current_valid_tick_array_start_index.to_be_bytes(),
            ],
            &pool_config.raydium_v3_program
        ).0
    );
    let mut max_array_size = 5;
    while max_array_size != 0 {
        let next_tick_array_index = pool_state
            .next_initialized_tick_array_start_index(
                &Some(*tickarray_bitmap_extension),
                current_valid_tick_array_start_index,
                zero_for_one
            )
            .unwrap();
        if next_tick_array_index.is_none() {
            break;
        }
        current_valid_tick_array_start_index = next_tick_array_index.unwrap();
        tick_array_keys.push(
            Pubkey::find_program_address(
                &[
                    raydium_amm_v3::states::TICK_ARRAY_SEED.as_bytes(),
                    pool_config.pool_id_account.unwrap().to_bytes().as_ref(),
                    &current_valid_tick_array_start_index.to_be_bytes(),
                ],
                &pool_config.raydium_v3_program
            ).0
        );
        max_array_size -= 1;
    }
    let tick_array_rsps = rpc_client.get_multiple_accounts(&tick_array_keys).unwrap();
    let mut tick_arrays = VecDeque::new();
    for tick_array in tick_array_rsps {
        let tick_array_state = instructions::utils
            ::deserialize_anchor_account::<raydium_amm_v3::states::TickArrayState>(
                &tick_array.unwrap()
            )
            .unwrap();
        tick_arrays.push_back(tick_array_state);
    }
    tick_arrays
}

pub fn get_all_nft_and_position_by_owner(
    client: &RpcClient,
    owner: &Pubkey,
    raydium_amm_v3_program: &Pubkey
) -> Vec<PositionNftTokenInfo> {
    let mut spl_nfts = get_nft_account_and_position_by_owner(
        client,
        owner,
        spl_token::id(),
        raydium_amm_v3_program
    );
    let spl_2022_nfts = get_nft_account_and_position_by_owner(
        client,
        owner,
        spl_token_2022::id(),
        raydium_amm_v3_program
    );
    spl_nfts.extend(spl_2022_nfts);
    spl_nfts
}

pub fn get_nft_account_and_position_by_owner(
    client: &RpcClient,
    owner: &Pubkey,
    token_program: Pubkey,
    raydium_amm_v3_program: &Pubkey
) -> Vec<PositionNftTokenInfo> {
    let all_tokens = client
        .get_token_accounts_by_owner(owner, TokenAccountsFilter::ProgramId(token_program))
        .unwrap();
    let mut position_nft_accounts = Vec::new();
    for keyed_account in all_tokens {
        if let UiAccountData::Json(parsed_account) = keyed_account.account.data {
            if parsed_account.program == "spl-token" || parsed_account.program == "spl-token-2022" {
                if
                    let Ok(TokenAccountType::Account(ui_token_account)) = serde_json::from_value(
                        parsed_account.parsed
                    )
                {
                    let _frozen = ui_token_account.state == UiAccountState::Frozen;

                    let token = ui_token_account.mint
                        .parse::<Pubkey>()
                        .unwrap_or_else(|err| panic!("Invalid mint: {}", err));
                    let token_account = keyed_account.pubkey
                        .parse::<Pubkey>()
                        .unwrap_or_else(|err| panic!("Invalid token account: {}", err));
                    let token_amount = ui_token_account.token_amount.amount
                        .parse::<u64>()
                        .unwrap_or_else(|err| panic!("Invalid token amount: {}", err));

                    let _close_authority = ui_token_account.close_authority.map_or(*owner, |s| {
                        s.parse::<Pubkey>().unwrap_or_else(|err|
                            panic!("Invalid close authority: {}", err)
                        )
                    });

                    if ui_token_account.token_amount.decimals == 0 && token_amount == 1 {
                        let (position_pda, _) = Pubkey::find_program_address(
                            &[
                                raydium_amm_v3::states::POSITION_SEED.as_bytes(),
                                token.to_bytes().as_ref(),
                            ],
                            &raydium_amm_v3_program
                        );
                        position_nft_accounts.push(PositionNftTokenInfo {
                            key: token_account,
                            program: token_program,
                            position: position_pda,
                            mint: token,
                            amount: token_amount,
                            decimals: ui_token_account.token_amount.decimals,
                        });
                    }
                }
            }
        }
    }
    position_nft_accounts
}
