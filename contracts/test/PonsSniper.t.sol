// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {PonsSniper} from "../src/PonsSniper.sol";
import {MockToken, MockCurve, ReentrantToken, FACTORY} from "./Mocks.sol";

contract PonsSniperTest is Test {
    PonsSniper sniper;
    address owner = address(0xA11CE);
    address recipient = address(0xB0B);
    address stranger = address(0xBAD);

    // A launch's opening curve, in the shape the real ones open at: a phantom
    // quote of two fifths of the graduation threshold against a supply of 1e9,
    // with two sevenths of that supply reserved for the graduated pool.
    uint256 constant PHANTOM = 1.68 ether;
    uint256 constant SUPPLY = 1_000_000_000 ether;
    uint256 constant RESERVED = (SUPPLY * 2) / 7;

    function setUp() public {
        // 19 bps: the third step of the schedule this launchpad is running,
        // which is where a launch stops being expensive.
        sniper = new PonsSniper(owner, FACTORY, 19);
        vm.deal(owner, 100 ether);
    }

    /// A curve opens at the current timestamp, and nothing may buy in the
    /// second a launch opened in - so every curve these tests trade against is
    /// made one second old, which is where a buy is allowed at all.
    function _opened(MockCurve curve) internal returns (MockCurve) {
        vm.warp(block.timestamp + 1);
        return curve;
    }

    function _nativeCurve() internal returns (MockCurve) {
        return _opened(new MockCurve(address(0), PHANTOM, SUPPLY, RESERVED));
    }

    function _tokenCurve(MockToken quote) internal returns (MockCurve) {
        return _opened(new MockCurve(address(quote), PHANTOM, SUPPLY, RESERVED));
    }

    /// The ordinary native buy: value in, tokens to the recipient, nothing
    /// left behind.
    function test_native_buy() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(19); // two seconds in

        vm.prank(owner);
        uint256 out = sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);

        assertGt(out, 0);
        assertEq(curve.launched().balanceOf(recipient), out);
        assertEq(address(sniper).balance, 0, "the wrapper kept something");
        assertEq(owner.balance, 99 ether);
    }

    /// The whole point of the wrapper on an ERC-20 launch: one call approves a
    /// curve that did not exist a second ago and buys from it.
    function test_erc20_buy_is_one_call() public {
        MockToken quote = new MockToken(0, false, false);
        quote.mint(owner, 10 ether);
        MockCurve curve = _tokenCurve(quote);

        // The one-time approval, of the wrapper rather than of any curve.
        vm.prank(owner);
        quote.approve(address(sniper), type(uint256).max);

        vm.prank(owner);
        uint256 out = sniper.snipe(address(curve), 1 ether, 0, recipient, 19, 0);

        assertGt(out, 0);
        assertEq(curve.launched().balanceOf(recipient), out);
        assertEq(quote.balanceOf(owner), 9 ether);
        // No allowance is left standing on a contract nobody has audited.
        assertEq(quote.allowance(address(sniper), address(curve)), 0);
        assertEq(quote.balanceOf(address(sniper)), 0);
    }

    /// Landing one second early is the entire risk of sniping a launch, and it
    /// must cost gas rather than money.
    function test_refuses_a_tax_above_the_limit() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(9900); // the launch second

        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.SnipeTaxTooHigh.selector, 9900, 19));
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);

        // Nothing was spent and nothing was approved.
        assertEq(owner.balance, 100 ether);
        assertEq(address(sniper).balance, 0);
    }

    /// An exempt recipient reads zero and passes the strictest limit there is.
    function test_an_exempt_recipient_passes() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(0);
        vm.prank(owner);
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 0, 0);
        assertGt(curve.launched().balanceOf(recipient), 0);
    }

    function test_deadline() public {
        MockCurve curve = _nativeCurve();
        curve.setLaunchedAt(1_788_816_990);
        vm.warp(1_788_817_000);
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.TooLate.selector, 1_788_817_000, 1_788_816_999));
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 10_000, 1_788_816_999);
    }

    /// The last buy of a launch is filled to the allocation and refunded the
    /// rest, and the refund is the owner's rather than the wrapper's.
    function test_a_clamped_fill_refunds_the_owner() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(0);

        vm.prank(owner);
        uint256 out = sniper.snipe{value: 50 ether}(address(curve), 50 ether, 0, recipient, 0, 0);

        assertEq(out, SUPPLY - RESERVED, "the fill was not clamped to the allocation");
        assertEq(address(sniper).balance, 0, "the refund stayed in the wrapper");
        assertGt(owner.balance, 50 ether, "the owner was not refunded");
    }

    /// Same, on the ERC-20 side, where the refund arrives as a transfer.
    function test_a_clamped_erc20_fill_refunds_the_owner() public {
        MockToken quote = new MockToken(0, false, false);
        quote.mint(owner, 100 ether);
        MockCurve curve = _tokenCurve(quote);
        vm.prank(owner);
        quote.approve(address(sniper), type(uint256).max);

        vm.prank(owner);
        sniper.snipe(address(curve), 50 ether, 0, recipient, 19, 0);

        assertEq(quote.balanceOf(address(sniper)), 0, "the refund stayed in the wrapper");
        assertGt(quote.balanceOf(owner), 50 ether, "the owner was not refunded");
        assertEq(quote.allowance(address(sniper), address(curve)), 0);
    }

    /// A quote asset that keeps part of every transfer delivers less than was
    /// asked for, and the buy has to be for what arrived.
    function test_a_fee_on_transfer_quote_spends_what_arrived() public {
        MockToken quote = new MockToken(100, false, false); // 1% kept
        quote.mint(owner, 10 ether);
        MockCurve curve = _tokenCurve(quote);
        vm.prank(owner);
        quote.approve(address(sniper), type(uint256).max);

        vm.prank(owner);
        sniper.snipe(address(curve), 1 ether, 0, recipient, 19, 0);

        assertEq(quote.balanceOf(address(sniper)), 0);
        assertEq(quote.allowance(address(sniper), address(curve)), 0);
    }

    /// Tokens that refuse a nonzero-to-nonzero approval, and tokens that
    /// return nothing at all. Both are in circulation and both would strand a
    /// launch quoted in them.
    function test_awkward_tokens() public {
        MockToken strict = new MockToken(0, true, false);
        strict.mint(owner, 10 ether);
        MockCurve curve = _tokenCurve(strict);
        vm.startPrank(owner);
        strict.approve(address(sniper), type(uint256).max);
        sniper.snipe(address(curve), 1 ether, 0, recipient, 19, 0);
        vm.stopPrank();
        assertEq(strict.allowance(address(sniper), address(curve)), 0);

        MockToken silent = new MockToken(0, false, true);
        silent.mint(owner, 10 ether);
        MockCurve curve2 = _tokenCurve(silent);
        vm.startPrank(owner);
        // Low level even here: a token that returns nothing cannot be called
        // through an interface that says it returns a bool, and that is the
        // whole point of this one.
        (bool ok,) = address(silent)
            .call(abi.encodeWithSignature("approve(address,uint256)", address(sniper), type(uint256).max));
        require(ok, "approve");
        sniper.snipe(address(curve2), 1 ether, 0, recipient, 19, 0);
        vm.stopPrank();
        assertGt(curve2.launched().balanceOf(recipient), 0);
    }

    /// An address with no code accepts every call and does nothing, which
    /// would make a transfer here a silent no-op.
    function test_a_token_with_no_code_is_refused() public {
        MockCurve curve = _opened(new MockCurve(address(0xDEAD), PHANTOM, SUPPLY, RESERVED));
        vm.prank(owner);
        vm.expectRevert(PonsSniper.TransferFailed.selector);
        sniper.snipe(address(curve), 1 ether, 0, recipient, 19, 0);
    }

    /// A quote asset with a transfer hook, re-entering while its own allowance
    /// is still standing.
    function test_reentrancy_is_refused() public {
        ReentrantToken quote = new ReentrantToken();
        quote.mint(owner, 10 ether);
        MockCurve curve = _tokenCurve(quote);
        vm.prank(owner);
        quote.approve(address(sniper), type(uint256).max);

        quote.arm(address(sniper), abi.encodeCall(PonsSniper.snipe, (address(curve), 1 ether, 0, recipient, 19, 0)));

        vm.prank(owner);
        // The inner call is not the owner - it is the token - so it fails on
        // ownership before the guard is even reached. Either refusal is a
        // refusal; what matters is that it does not go through.
        vm.expectRevert();
        sniper.snipe(address(curve), 1 ether, 0, recipient, 19, 0);
    }

    /// The rule with no setting: not in the second the launch opened in.
    ///
    /// Not expressed as a limit in basis points, so it holds whatever the
    /// schedule is retuned to - and it holds with the ceiling opened all the
    /// way, with a curve reporting no tax at all, and with the caller asking
    /// for anything.
    function test_the_launch_second_cannot_be_bought_by_anyone() public {
        MockCurve curve = _nativeCurve();
        curve.setLaunchedAt(block.timestamp); // this very second
        curve.setTax(0); // and the curve says it is free

        vm.prank(owner);
        sniper.setSnipeTaxCeiling(type(uint256).max);

        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.TheLaunchSecond.selector, block.timestamp, block.timestamp));
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, type(uint256).max, 0);

        assertEq(owner.balance, 100 ether, "money moved in the launch second");

        // One second later the same call goes through.
        vm.warp(block.timestamp + 1);
        vm.prank(owner);
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, type(uint256).max, 0);
        assertGt(curve.launched().balanceOf(recipient), 0);
    }

    /// A curve that has not opened has no launch second to be after.
    function test_a_curve_that_has_not_opened_is_refused() public {
        MockCurve curve = _nativeCurve();
        curve.setLaunchedAt(0);
        vm.prank(owner);
        vm.expectRevert(PonsSniper.CurveNotOpen.selector);
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);
    }

    /// The ceiling is what stops a caller\'s own mistake, and it is a decision
    /// rather than a law: the owner can open it when they mean to pay a step.
    /// The policy is a deployment decision, recorded in that deployment\'s own
    /// logs rather than carried as a constant in the source.
    function test_the_ceiling_is_declared_at_deployment() public {
        PonsSniper strict = new PonsSniper(owner, FACTORY, 0);
        assertEq(strict.snipeTaxCeilingBps(), 0);

        MockCurve curve = _nativeCurve();
        curve.setTax(19);
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.TaxCeilingExceeded.selector, 19, 0));
        strict.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);

        // A zero ceiling still buys, at a tax of zero: three seconds in, or
        // from an address the launcher exempted.
        curve.setTax(0);
        vm.prank(owner);
        strict.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 0, 0);
        assertGt(curve.launched().balanceOf(recipient), 0);
    }

    function test_the_ceiling_stops_a_caller_mistake() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(618);

        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.TaxCeilingExceeded.selector, 618, 19));
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 618, 0);

        // Even inside the ceiling, the curve\'s own answer still has to fit.
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.SnipeTaxTooHigh.selector, 618, 19));
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);

        // Opened deliberately, the step is reachable.
        vm.prank(owner);
        sniper.setSnipeTaxCeiling(618);
        vm.prank(owner);
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 618, 0);
        assertGt(curve.launched().balanceOf(recipient), 0);

        vm.prank(stranger);
        vm.expectRevert(PonsSniper.NotOwner.selector);
        sniper.setSnipeTaxCeiling(0);
    }

    /// The mistake an owner actually makes: the wrong address. A token instead
    /// of its curve, or one left over from the launch before.
    function test_a_curve_from_another_factory_is_refused() public {
        MockCurve curve = _nativeCurve();
        curve.setFactory(address(0xF00D));
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.NotAPonsCurve.selector, address(curve), address(0xF00D)));
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);
        assertEq(owner.balance, 100 ether, "money moved for a curve we do not know");
    }

    function test_only_the_owner_trades() public {
        MockCurve curve = _nativeCurve();
        vm.deal(stranger, 1 ether);
        vm.prank(stranger);
        vm.expectRevert(PonsSniper.NotOwner.selector);
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);
    }

    function test_native_value_must_match() public {
        MockCurve curve = _nativeCurve();
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.NativeValueMismatch.selector, 0.5 ether, 1 ether));
        sniper.snipe{value: 0.5 ether}(address(curve), 1 ether, 0, recipient, 19, 0);
    }

    function test_no_value_on_an_erc20_launch() public {
        MockToken quote = new MockToken(0, false, false);
        MockCurve curve = _tokenCurve(quote);
        vm.prank(owner);
        vm.expectRevert(PonsSniper.UnexpectedNativeValue.selector);
        sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, recipient, 19, 0);
    }

    function test_rescue() public {
        vm.deal(address(sniper), 3 ether);
        vm.prank(owner);
        sniper.rescue(address(0), owner);
        assertEq(address(sniper).balance, 0);
        assertEq(owner.balance, 103 ether);

        MockToken t = new MockToken(0, false, false);
        t.mint(address(sniper), 5 ether);
        vm.prank(owner);
        sniper.rescue(address(t), owner);
        assertEq(t.balanceOf(owner), 5 ether);

        vm.prank(stranger);
        vm.expectRevert(PonsSniper.NotOwner.selector);
        sniper.rescue(address(0), stranger);
    }

    /// Two steps, so a mistyped address cannot take the contract - and the
    /// standing quote-token approvals with it - out of reach.
    function test_ownership_is_handed_over_in_two_steps() public {
        vm.prank(owner);
        sniper.transferOwnership(stranger);
        assertEq(sniper.owner(), owner, "ownership moved on the first step");

        vm.prank(recipient);
        vm.expectRevert(PonsSniper.NotPendingOwner.selector);
        sniper.acceptOwnership();

        vm.prank(stranger);
        sniper.acceptOwnership();
        assertEq(sniper.owner(), stranger);
    }
    // ---- unwind: the way out ----------------------------------------------
    //
    // A position bought to the wrapper is sold from it, which is the whole
    // reason the wrapper holds one at all: the curve pulls the launched token
    // with `transferFrom`, and that token does not exist until the launch, so
    // an EOA would need an approval and then a sale. The exit measured on the
    // journals is worth what it is because it lands within a block of the
    // price that triggered it, and two transactions are not one block.

    /// Buy to the wrapper, sell from it, and the proceeds reach the owner.
    function test_unwind_pays_the_owner() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(19);
        vm.prank(owner);
        uint256 bought = sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, address(sniper), 19, 0);
        assertEq(curve.launched().balanceOf(address(sniper)), bought);

        uint256 before = owner.balance;
        vm.prank(owner);
        uint256 out = sniper.unwind(address(curve), bought, 0, 0);

        assertGt(out, 0);
        assertEq(owner.balance - before, out, "the owner was not paid");
        assertEq(curve.launched().balanceOf(address(sniper)), 0, "a position was left behind");
        assertEq(
            curve.launched().allowance(address(sniper), address(curve)),
            0,
            "an allowance was left standing"
        );
    }

    /// Zero means the whole balance, which is the ordinary case: an exact
    /// figure a wei off what is held would revert, and a revert on the way out
    /// keeps a position while the price it was leaving falls.
    function test_unwind_zero_sells_everything() public {
        MockToken quote = new MockToken(0, false, false);
        MockCurve curve = _tokenCurve(quote);
        curve.setTax(19);
        quote.mint(owner, 10 ether);
        vm.prank(owner);
        quote.approve(address(sniper), type(uint256).max);
        vm.prank(owner);
        sniper.snipe(address(curve), 1 ether, 0, address(sniper), 19, 0);

        uint256 held = curve.launched().balanceOf(address(sniper));
        assertGt(held, 0);
        uint256 before = quote.balanceOf(owner);

        vm.prank(owner);
        uint256 out = sniper.unwind(address(curve), 0, 0, 0);

        assertEq(curve.launched().balanceOf(address(sniper)), 0);
        assertEq(quote.balanceOf(owner) - before, out);
    }

    /// Part of a position, when that is what was asked for.
    function test_unwind_sells_part() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(19);
        vm.prank(owner);
        uint256 bought = sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, address(sniper), 19, 0);

        vm.prank(owner);
        sniper.unwind(address(curve), bought / 2, 0, 0);
        assertEq(curve.launched().balanceOf(address(sniper)), bought - bought / 2);
    }

    /// The minimum is the curve's to enforce, and it does.
    function test_unwind_respects_the_minimum() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(19);
        vm.prank(owner);
        uint256 bought = sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, address(sniper), 19, 0);

        vm.prank(owner);
        vm.expectRevert();
        sniper.unwind(address(curve), bought, 100 ether, 0);
        // And the position is still here, unspent, with nothing approved.
        assertEq(curve.launched().balanceOf(address(sniper)), bought);
        assertEq(curve.launched().allowance(address(sniper), address(curve)), 0);
    }

    /// The mistakes that actually happen: a wrong curve, an empty one, an
    /// amount larger than the position, a deadline gone by, and somebody else.
    function test_unwind_refuses_the_mistakes() public {
        MockCurve curve = _nativeCurve();
        curve.setTax(19);
        vm.prank(owner);
        uint256 bought = sniper.snipe{value: 1 ether}(address(curve), 1 ether, 0, address(sniper), 19, 0);

        vm.prank(stranger);
        vm.expectRevert(PonsSniper.NotOwner.selector);
        sniper.unwind(address(curve), bought, 0, 0);

        vm.prank(owner);
        vm.expectRevert(PonsSniper.ZeroAddress.selector);
        sniper.unwind(address(0), bought, 0, 0);

        vm.prank(owner);
        vm.expectRevert(
            abi.encodeWithSelector(PonsSniper.MoreThanHeld.selector, bought + 1, bought)
        );
        sniper.unwind(address(curve), bought + 1, 0, 0);

        vm.prank(owner);
        vm.expectRevert(
            abi.encodeWithSelector(PonsSniper.TooLate.selector, block.timestamp, block.timestamp - 1)
        );
        sniper.unwind(address(curve), bought, 0, block.timestamp - 1);

        // A curve that does not answer to our factory is not one we hand a
        // position to - the same check the buy makes, for the same reason.
        MockCurve other = _nativeCurve();
        other.setFactory(address(0xDEAD));
        vm.prank(owner);
        vm.expectRevert(
            abi.encodeWithSelector(PonsSniper.NotAPonsCurve.selector, address(other), address(0xDEAD))
        );
        sniper.unwind(address(other), bought, 0, 0);

        // Nothing of it was spent by any of that.
        assertEq(curve.launched().balanceOf(address(sniper)), bought);
    }

    /// Selling a position that is not here is a mistake worth naming rather
    /// than a sale of nothing.
    function test_unwind_with_nothing_held() public {
        MockCurve curve = _nativeCurve();
        address launched = address(curve.launched());
        vm.prank(owner);
        vm.expectRevert(abi.encodeWithSelector(PonsSniper.NothingHeld.selector, launched));
        sniper.unwind(address(curve), 0, 0, 0);
    }

    /// A quote asset that keeps a cut of every transfer pays the owner less
    /// than the curve says it sent, and there is nothing this contract can do
    /// about that - the token is between them. Written down because the number
    /// in the event is the curve's, not the owner's, and reading it as the
    /// owner's would overstate every trade in such a token by the fee.
    function test_unwind_in_a_fee_on_transfer_quote_pays_the_owner_less() public {
        MockToken quote = new MockToken(18, false, false); // 0.18% on transfer
        MockCurve curve = _tokenCurve(quote);
        curve.setTax(19);
        quote.mint(owner, 10 ether);
        vm.prank(owner);
        quote.approve(address(sniper), type(uint256).max);
        vm.prank(owner);
        sniper.snipe(address(curve), 1 ether, 0, address(sniper), 19, 0);

        uint256 before = quote.balanceOf(owner);
        vm.prank(owner);
        uint256 out = sniper.unwind(address(curve), 0, 0, 0);

        uint256 arrived = quote.balanceOf(owner) - before;
        assertLt(arrived, out, "the fee went missing somewhere it should not have");
        assertEq(arrived, out - (out * 18) / 10_000);
        assertEq(curve.launched().balanceOf(address(sniper)), 0);
    }

}
