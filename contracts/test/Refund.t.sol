// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {PonsSniper} from "../src/PonsSniper.sol";
import {FACTORY} from "./Mocks.sol";

/**
 * @title The native refund has to arrive on whatever gas the curve forwards
 * @notice A clamped native fill sends change back here, and a curve that
 * refunds with `transfer` or `send` forwards 2300 gas and not a unit more. An
 * empty `receive()` fits inside that. Almost nothing else does - a single cold
 * SLOAD is 2100 of it, and putting this contract behind a proxy would spend
 * the whole stipend on the delegatecall alone.
 *
 * So this measures the margin rather than trusting it: a failed refund is not
 * a lost refund, it reverts the entire buy.
 */
contract RefundGasTest is Test {
    PonsSniper sniper;

    function setUp() public {
        sniper = new PonsSniper(address(0xA11CE), FACTORY, 19);
        vm.deal(address(this), 10 ether);
        // Warm the account, the way a real refund finds it: the curve was
        // called from here a moment ago.
        _send(1 wei, gasleft());
    }

    /// `gas: 0` with a nonzero value is exactly `transfer` and `send`: the
    /// callee runs on the 2300 stipend the EVM adds, and nothing more.
    function test_the_refund_fits_the_stipend() public {
        assertTrue(_send(1 ether, 0), "receive() no longer fits a 2300 gas refund");
    }

    /// What it actually needs, bounded rather than pinned - the number moves
    /// with the compiler, the claim does not.
    function test_the_margin_is_measured() public {
        uint256 lo = 0;
        uint256 hi = 20_000;
        while (lo < hi) {
            uint256 mid = (lo + hi) / 2;
            if (_send(1 wei, mid)) hi = mid;
            else lo = mid + 1;
        }
        emit log_named_uint("gas the refund needs beyond the stipend", lo);
        assertEq(lo, 0, "receive() got heavier - a curve refunding with transfer would now fail");
    }

    function _send(uint256 value, uint256 gas) internal returns (bool ok) {
        (ok,) = payable(address(sniper)).call{value: value, gas: gas}("");
    }
}
