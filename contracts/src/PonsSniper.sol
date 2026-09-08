// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/**
 * @title PonsSniper
 * @notice One transaction that approves a brand-new PonsV2 bonding curve and
 * buys from it, with the launch window's own terms as the conditions.
 *
 * A native launch needs none of this: `buy` is payable and takes ETH straight
 * from an EOA. An ERC-20 launch does, and the reason is timing rather than
 * convenience. The curve pulls its quote asset with `transferFrom` and knows
 * nothing about Permit2, so a buyer must approve it first - and the curve's
 * address does not exist until the launch transaction itself. That approval is
 * therefore a second transaction inside a three second tax window, which is
 * the window. Holding the approval here instead makes it one call: the owner
 * approves THIS contract once per quote token, ever, and every launch after
 * that is a single transaction.
 *
 * The conditions are what the money actually cares about, so they are stated
 * that way rather than as block numbers or seconds:
 *
 * - `maxSnipeTaxBps` - refuse if the curve would charge the recipient more
 *   than this. The launch second costs 99% and the second after it 6.18%, so
 *   landing one second early is the whole risk of sniping a launch, and this
 *   is that risk expressed exactly. It is read from the curve, which knows
 *   about exemptions and about its own frozen terms, rather than computed from
 *   a clock this contract would have to be told about.
 * - `minTokensOut` - handed to the curve, which enforces it as a price bound
 *   on a clamped fill and as a quantity bound otherwise.
 * - `notAfter` - a deadline, for a transaction that sits in a queue and
 *   arrives at a launch that has moved on.
 *
 * Above all of them sits one rule with no setting: **this never buys in the
 * second a launch opened in.** That second costs 99% of the trade, and it is
 * the one price no argument, no configuration and no mistake should be able to
 * reach. It is written as `block.timestamp > launchedAt` rather than as a limit
 * in basis points, because that is what the rule actually is - a tax schedule
 * can be retuned and its numbers move, the launch second cannot. Every step
 * after it is a decision, and decisions belong in `snipeTaxCeilingBps` and in
 * the argument.
 *
 * The tax is charged on the RECIPIENT, not on the caller, so `recipient` is
 * what decides both what is paid and where the tokens land.
 *
 * The curve is checked against the factory it claims, because the one mistake
 * an owner can actually make here is passing the wrong address - a token
 * instead of its curve, or a stale one from the launch before. It is not a
 * defence against a caller who wants to be robbed: a contract that lies about
 * its factory can lie about everything else too, and only the owner can call
 * this at all. It is a defence against a typo, which is the failure that
 * happens.
 *
 * Nothing is held here between trades. Quote assets are pulled from the owner
 * when a buy is made and whatever the curve refunds goes straight back, so a
 * key compromised tomorrow finds an empty contract.
 */
contract PonsSniper {
    error NotOwner();
    error NotPendingOwner();
    error ZeroAddress();
    error ZeroAmount();
    error Reentrancy();
    error TooLate(uint256 nowAt, uint256 notAfter);
    error SnipeTaxTooHigh(uint256 taxBps, uint256 maxBps);
    error NativeValueMismatch(uint256 sent, uint256 expected);
    error UnexpectedNativeValue();
    error TransferFailed();
    error NothingReceived();
    error TaxCeilingExceeded(uint256 requested, uint256 ceiling);
    error NotAPonsCurve(address curve, address itsFactory);
    error CurveNotOpen();
    error TheLaunchSecond(uint256 nowAt, uint256 launchedAt);

    event Sniped(
        address indexed curve,
        address indexed recipient,
        address quoteToken,
        uint256 spent,
        uint256 tokensOut,
        uint256 taxBps
    );
    event Rescued(address indexed asset, address indexed to, uint256 amount);
    event SnipeTaxCeilingUpdated(uint256 ceilingBps);
    event OwnershipTransferStarted(address indexed previousOwner, address indexed newOwner);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    /// The launch factory every curve this will buy from must answer to.
    address public immutable factory;

    address public owner;
    address public pendingOwner;

    /**
     * @notice The most any single call may agree to pay, so a mistake in
     * whatever computes the argument cannot buy an expensive step by accident.
     *
     * Set at deployment rather than defaulted here. Any number worth putting
     * in this slot comes from the schedule the factory is running - 19 bps is
     * the third step of the current one, where a launch stops being expensive
     * - and a schedule can be retuned. A default in the source would go on
     * looking deliberate long after it stopped being true, so the policy is
     * stated once, on purpose, and recorded in the deployment's own logs.
     *
     * The launch second is not reachable through this at any value.
     */
    uint256 public snipeTaxCeilingBps;

    // The curve pays a native refund with a plain `call`, which lands in
    // `receive()` below and re-enters nothing - but an ERC-20 quote asset with
    // a transfer hook can re-enter this contract in the middle of a buy, while
    // its allowance is still standing.
    uint256 private locked = 1;

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier nonReentrant() {
        if (locked != 1) revert Reentrancy();
        locked = 2;
        _;
        locked = 1;
    }

    constructor(address owner_, address factory_, uint256 snipeTaxCeilingBps_) {
        if (owner_ == address(0) || factory_ == address(0)) revert ZeroAddress();
        owner = owner_;
        factory = factory_;
        snipeTaxCeilingBps = snipeTaxCeilingBps_;
        emit OwnershipTransferred(address(0), owner_);
        emit SnipeTaxCeilingUpdated(snipeTaxCeilingBps_);
    }

    /**
     * @notice Buys `quoteIn` worth of a launch from its curve, in one call.
     * @param curve The launch's bonding curve.
     * @param quoteIn Amount of the curve's quote asset to spend. For a native
     * launch this must equal `msg.value`; for an ERC-20 launch no value may be
     * sent and the amount is pulled from the owner.
     * @param minTokensOut Passed to the curve unchanged.
     * @param recipient Who receives the tokens, and whose snipe tax applies.
     * @param maxSnipeTaxBps Refuse the buy if the curve would charge the
     * recipient more than this. Pass 10000 to accept any tax.
     * @param notAfter Latest timestamp this may execute at; zero for none.
     */
    function snipe(
        address curve,
        uint256 quoteIn,
        uint256 minTokensOut,
        address recipient,
        uint256 maxSnipeTaxBps,
        uint256 notAfter
    ) external payable onlyOwner nonReentrant returns (uint256 tokensOut) {
        if (curve == address(0) || recipient == address(0)) revert ZeroAddress();
        if (quoteIn == 0) revert ZeroAmount();
        if (notAfter != 0 && block.timestamp > notAfter) revert TooLate(block.timestamp, notAfter);
        // Loud rather than clamped: a caller that asked to accept 618 bps has
        // a wrong idea about what this contract does, and finding that out
        // here beats finding it out from a fill that looks like a bad price.
        if (maxSnipeTaxBps > snipeTaxCeilingBps) {
            revert TaxCeilingExceeded(maxSnipeTaxBps, snipeTaxCeilingBps);
        }

        // Before anything is read that would be trusted: a curve that does not
        // answer to the launch factory is not a curve this will hand money to.
        address itsFactory = IPonsV2BondingCurve(curve).factory();
        if (itsFactory != factory) revert NotAPonsCurve(curve, itsFactory);

        // The rule with no setting. Not "the tax is under some number" - that
        // number moves when the schedule is retuned - but the second itself,
        // which is what costs 99% and is what nothing here may buy in.
        uint256 launchedAt = IPonsV2BondingCurve(curve).launchedAt();
        if (launchedAt == 0) revert CurveNotOpen();
        if (block.timestamp <= launchedAt) revert TheLaunchSecond(block.timestamp, launchedAt);

        // Asked before anything is spent or approved, so a buy landing in the
        // wrong second costs gas and nothing else. The curve answers for this
        // recipient specifically, exemptions included.
        uint256 taxBps = IPonsV2BondingCurve(curve).currentSnipeTaxBps(recipient);
        if (taxBps > maxSnipeTaxBps) revert SnipeTaxTooHigh(taxBps, maxSnipeTaxBps);

        // Read from the curve rather than taken as an argument: an argument
        // that disagreed with the curve would approve one token and buy with
        // another, and the mistake would only surface as a stuck allowance.
        address quoteToken = IPonsV2BondingCurve(curve).pairToken();

        uint256 spent;
        if (quoteToken == address(0)) {
            if (msg.value != quoteIn) revert NativeValueMismatch(msg.value, quoteIn);
            // Anything held before this call funds itself is not this call's to
            // refund. Measured rather than swept, so dust left by an earlier
            // trade is never paid out as if it were change.
            uint256 balanceBefore = address(this).balance - msg.value;
            spent = quoteIn;
            tokensOut = IPonsV2BondingCurve(curve).buy{value: quoteIn}(quoteIn, minTokensOut, recipient);
            uint256 refund = address(this).balance - balanceBefore;
            if (refund != 0) {
                spent -= refund;
                _sendNative(owner, refund);
            }
        } else {
            if (msg.value != 0) revert UnexpectedNativeValue();
            uint256 balanceBefore = _balanceOf(quoteToken, address(this));
            _safeTransferFrom(quoteToken, owner, address(this), quoteIn);
            // What arrived, not what was asked for: a fee-on-transfer quote
            // asset delivers less, and approving more than is held would leave
            // an allowance standing after a buy that could not use it.
            uint256 received = _balanceOf(quoteToken, address(this)) - balanceBefore;
            if (received == 0) revert NothingReceived();

            _forceApprove(quoteToken, curve, received);
            spent = received;
            tokensOut = IPonsV2BondingCurve(curve).buy(received, minTokensOut, recipient);
            // A clamped fill leaves the unused allowance standing, and a
            // standing allowance on a contract nobody has audited is a
            // liability for as long as it lasts.
            _forceApprove(quoteToken, curve, 0);

            uint256 refund = _balanceOf(quoteToken, address(this)) - balanceBefore;
            if (refund != 0) {
                spent -= refund;
                _safeTransfer(quoteToken, owner, refund);
            }
        }

        emit Sniped(curve, recipient, quoteToken, spent, tokensOut, taxBps);
    }

    /**
     * @notice Moves the limit any single call may agree to.
     *
     * Unbounded on purpose. Every step this can now reach is one a launch may
     * still be worth buying at, and which of them is worth it depends on the
     * flow in that second rather than on anything decidable here. The step
     * that is never worth it is the launch second, and that is refused
     * structurally rather than by this number.
     */
    function setSnipeTaxCeiling(uint256 ceilingBps) external onlyOwner {
        snipeTaxCeilingBps = ceilingBps;
        emit SnipeTaxCeilingUpdated(ceilingBps);
    }

    /**
     * @notice Sends the whole balance of `asset` here to `to`. The zero
     * address is the native currency.
     * @dev Nothing is meant to live here between trades. This exists for what
     * arrives anyway: a refund from a transaction that reverted downstream, a
     * token sent by mistake, tokens bought with this contract as recipient.
     */
    function rescue(address asset, address to) external onlyOwner {
        if (to == address(0)) revert ZeroAddress();
        uint256 amount;
        if (asset == address(0)) {
            amount = address(this).balance;
            if (amount != 0) _sendNative(to, amount);
        } else {
            amount = _balanceOf(asset, address(this));
            if (amount != 0) _safeTransfer(asset, to, amount);
        }
        emit Rescued(asset, to, amount);
    }

    /**
     * @notice Hands ownership over in two steps, so a mistyped address cannot
     * take the contract - and with it the standing quote-token approvals - out
     * of reach.
     */
    function transferOwnership(address newOwner) external onlyOwner {
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(owner, newOwner);
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert NotPendingOwner();
        emit OwnershipTransferred(owner, pendingOwner);
        owner = pendingOwner;
        pendingOwner = address(0);
    }

    /// Native refunds from a clamped fill arrive here, on the 2300 gas
    /// stipend a curve refunding with `transfer` or `send` forwards - which
    /// an empty `receive` fits inside and little else does. See
    /// `test/Refund.t.sol`.
    receive() external payable {}

    function _sendNative(address to, uint256 amount) private {
        (bool sent,) = payable(to).call{value: amount}("");
        if (!sent) revert TransferFailed();
    }

    // ERC-20 calls, written out rather than pulled in: this contract has no
    // dependencies, and the three quirks that matter - no return value, a
    // `false` return, and a non-contract address - are all handled here.

    function _balanceOf(address token, address who) private view returns (uint256) {
        (bool ok, bytes memory data) = token.staticcall(abi.encodeWithSelector(0x70a08231, who)); // balanceOf(address)
        if (!ok || data.length < 32) revert TransferFailed();
        return abi.decode(data, (uint256));
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        _call(token, abi.encodeWithSelector(0xa9059cbb, to, amount)); // transfer
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        _call(token, abi.encodeWithSelector(0x23b872dd, from, to, amount)); // transferFrom
    }

    /**
     * @dev Sets the allowance to zero first when it is not already, for the
     * tokens that refuse a change from one nonzero value to another.
     */
    function _forceApprove(address token, address spender, uint256 amount) private {
        (bool ok, bytes memory data) = token.call(abi.encodeWithSelector(0x095ea7b3, spender, amount)); // approve
        if (!ok || (data.length != 0 && !abi.decode(data, (bool)))) {
            _call(token, abi.encodeWithSelector(0x095ea7b3, spender, uint256(0)));
            _call(token, abi.encodeWithSelector(0x095ea7b3, spender, amount));
        }
    }

    function _call(address token, bytes memory data) private {
        (bool ok, bytes memory returned) = token.call(data);
        // A token with no code returns success and no data, which would make
        // every transfer here a silent no-op.
        if (!ok || (returned.length != 0 && !abi.decode(returned, (bool))) || token.code.length == 0) {
            revert TransferFailed();
        }
    }
}

interface IPonsV2BondingCurve {
    function buy(uint256 quoteIn, uint256 minTokensOut, address recipient) external payable returns (uint256 tokensOut);
    function pairToken() external view returns (address);
    function factory() external view returns (address);
    function launchedAt() external view returns (uint256);
    function currentSnipeTaxBps(address recipient) external view returns (uint256);
}
