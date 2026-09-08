//! The two calls this bot makes to its own contract.
//!
//! Nothing decides anything here and nothing is sent: these turn a decision
//! into the bytes that express it. What the bytes mean is in
//! `contracts/src/PonsSniper.sol`, and the arguments are named the same way on
//! both sides so the two can be read against each other.
//!
//! Both are built against the wrapper rather than the curve, and the reason is
//! the same for each. A buy needs the quote asset approved to the curve, and
//! the curve's address does not exist until the launch transaction - so an EOA
//! would approve and then buy, two transactions inside a three second window.
//! A sale needs the launched token approved, and that token does not exist
//! until the launch either - so an EOA would approve and then sell, two
//! transactions against a stop that is worth what it is because it lands
//! within a block.

use crate::swap::PendingTx;
use ethers::abi::{encode, Token};
use ethers::types::{Address, Bytes, U256};

const SNIPE: &str = "snipe(address,uint256,uint256,address,uint256,uint256)";
const UNWIND: &str = "unwind(address,uint256,uint256,uint256)";

/// Buy from a curve through the wrapper.
///
/// `value` carries the spend, which is what a native launch wants and the only
/// kind this trades. An ERC-20 quote would send zero value and have the
/// wrapper pull the amount from the owner instead; the contract handles both,
/// and this does not, because the analysis says every other pair together is
/// worth two percent of a result that is itself inside the noise.
///
/// `recipient` decides two things at once and they are the same thing: where
/// the tokens land and whose snipe tax the curve charges. It is the wrapper's
/// own address, because a position that lands anywhere else cannot be sold in
/// one transaction.
pub fn snipe(
    wrapper: Address,
    curve: Address,
    spend: U256,
    min_tokens_out: U256,
    max_tax_bps: u64,
    not_after: u64,
) -> PendingTx {
    let mut data = crate::pool::selector(SNIPE).to_vec();
    data.extend(encode(&[
        Token::Address(curve),
        Token::Uint(spend),
        Token::Uint(min_tokens_out),
        // The wrapper buys for itself. See the note above.
        Token::Address(wrapper),
        Token::Uint(U256::from(max_tax_bps)),
        Token::Uint(U256::from(not_after)),
    ]));
    PendingTx {
        label: format!("snipe {curve:?} for {spend} wei"),
        to: wrapper,
        data: Bytes::from(data),
        value: spend,
    }
}

/// Sell a position back to the curve it came from.
///
/// `tokens_in` of zero means the whole balance, and that is what this passes
/// unless told otherwise: an exact figure a wei off what the wrapper holds
/// reverts, and a revert on the way out keeps a position while the price it
/// was leaving falls.
pub fn unwind(
    wrapper: Address,
    curve: Address,
    tokens_in: U256,
    min_quote_out: U256,
    not_after: u64,
) -> PendingTx {
    let mut data = crate::pool::selector(UNWIND).to_vec();
    data.extend(encode(&[
        Token::Address(curve),
        Token::Uint(tokens_in),
        Token::Uint(min_quote_out),
        Token::Uint(U256::from(not_after)),
    ]));
    PendingTx {
        label: format!("unwind {curve:?}"),
        to: wrapper,
        data: Bytes::from(data),
        value: U256::zero(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::abi::ParamType;

    fn addr(n: u8) -> Address {
        let mut b = [0u8; 20];
        b[19] = n;
        Address::from(b)
    }

    /// The signatures, spelled out. A selector built from a signature that
    /// does not match the deployed contract's is four bytes of silence: the
    /// call reverts as an unknown function, and it reverts in the second the
    /// whole thing was aimed at.
    #[test]
    fn the_selectors_are_the_hashes_of_the_signatures() {
        assert_eq!(
            hex::encode(crate::pool::selector(SNIPE)),
            hex::encode(&ethers::utils::keccak256(SNIPE.as_bytes())[..4])
        );
        assert_eq!(
            hex::encode(crate::pool::selector(UNWIND)),
            hex::encode(&ethers::utils::keccak256(UNWIND.as_bytes())[..4])
        );
    }

    /// What goes in comes back out, in the order the contract reads it. The
    /// mistake this catches is two arguments of the same type swapped - a
    /// minimum where a tax ceiling belongs encodes perfectly and means
    /// something else entirely.
    #[test]
    fn a_buy_encodes_the_arguments_the_contract_reads() {
        let tx = snipe(addr(1), addr(2), U256::from(1_000u64), U256::from(7u64), 19, 1_788_888_888);
        assert_eq!(tx.to, addr(1), "sent somewhere other than the wrapper");
        assert_eq!(tx.value, U256::from(1_000u64), "the spend is the value");
        assert_eq!(&tx.data[..4], &crate::pool::selector(SNIPE)[..]);

        let got = ethers::abi::decode(
            &[
                ParamType::Address,
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Address,
                ParamType::Uint(256),
                ParamType::Uint(256),
            ],
            &tx.data[4..],
        )
        .expect("our own encoding does not decode");
        assert_eq!(got[0], Token::Address(addr(2)), "curve");
        assert_eq!(got[1], Token::Uint(U256::from(1_000u64)), "spend");
        assert_eq!(got[2], Token::Uint(U256::from(7u64)), "min tokens out");
        // The one that matters: the wrapper buys for itself, or the position
        // lands where it cannot be sold in one transaction.
        assert_eq!(got[3], Token::Address(addr(1)), "recipient is the wrapper");
        assert_eq!(got[4], Token::Uint(U256::from(19u64)), "max tax bps");
        assert_eq!(got[5], Token::Uint(U256::from(1_788_888_888u64)), "not after");
    }

    #[test]
    fn a_sale_encodes_the_arguments_the_contract_reads() {
        let tx = unwind(addr(1), addr(2), U256::zero(), U256::from(500u64), 42);
        assert_eq!(tx.to, addr(1));
        assert!(tx.value.is_zero(), "a sale sends no value");
        assert_eq!(&tx.data[..4], &crate::pool::selector(UNWIND)[..]);

        let got = ethers::abi::decode(
            &[
                ParamType::Address,
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
            ],
            &tx.data[4..],
        )
        .expect("our own encoding does not decode");
        assert_eq!(got[0], Token::Address(addr(2)), "curve");
        // Zero is the whole balance, deliberately.
        assert_eq!(got[1], Token::Uint(U256::zero()), "tokens in");
        assert_eq!(got[2], Token::Uint(U256::from(500u64)), "min quote out");
        assert_eq!(got[3], Token::Uint(U256::from(42u64)), "not after");
    }
}
