use solomka_program::pubkey::Pubkey;
use solomka_sdk::account::Account;

#[derive(Debug)]
pub struct TokenAccountCookie {
    pub address: Pubkey,
}

#[derive(Debug)]
pub struct WalletCookie {
    pub address: Pubkey,
    pub account: Account,
}
