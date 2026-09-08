// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// A plain ERC-20, with the two behaviours that break naive wrappers built in.
contract MockToken {
    string public name = "Mock";
    uint8 public decimals = 18;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    /// Basis points kept by the token on every transfer.
    uint256 public transferFeeBps;
    /// Refuse a change from one nonzero allowance to another, as USDT does.
    bool public strictApprove;
    /// Return nothing at all, as the older tokens do.
    bool public silent;

    constructor(uint256 transferFeeBps_, bool strictApprove_, bool silent_) {
        transferFeeBps = transferFeeBps_;
        strictApprove = strictApprove_;
        silent = silent_;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        if (strictApprove) {
            require(amount == 0 || allowance[msg.sender][spender] == 0, "unsafe approve");
        }
        allowance[msg.sender][spender] = amount;
        if (silent) {
            assembly {
                return(0, 0)
            }
        }
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _move(msg.sender, to, amount);
        if (silent) {
            assembly {
                return(0, 0)
            }
        }
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external virtual returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        require(allowed >= amount, "allowance");
        if (allowed != type(uint256).max) allowance[from][msg.sender] = allowed - amount;
        _move(from, to, amount);
        if (silent) {
            assembly {
                return(0, 0)
            }
        }
        return true;
    }

    function _move(address from, address to, uint256 amount) internal {
        require(balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount - (amount * transferFeeBps) / 10_000;
    }
}

address constant FACTORY = 0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e;

/// The parts of PonsV2BondingCurve a buyer touches: the tax it would charge,
/// the asset it trades in, and a buy that prices, clamps and refunds the way
/// the real one does.
contract MockCurve {
    address public factory;
    address public pairToken;
    uint256 public quoteReserve;
    uint256 public tokenReserve;
    uint256 public reservedTokens;
    uint256 public taxBps;
    uint256 public launchedAt;
    MockToken public immutable launched;

    error SlippageExceeded(uint256 actual, uint256 minimum);

    constructor(address pairToken_, uint256 quoteReserve_, uint256 tokenReserve_, uint256 reserved_) {
        factory = FACTORY;
        launchedAt = block.timestamp;
        pairToken = pairToken_;
        quoteReserve = quoteReserve_;
        tokenReserve = tokenReserve_;
        reservedTokens = reserved_;
        launched = new MockToken(0, false, false);
        launched.mint(address(this), tokenReserve_);
    }

    function setFactory(address f) external {
        factory = f;
    }

    function setLaunchedAt(uint256 t) external {
        launchedAt = t;
    }

    function setTax(uint256 bps) external {
        taxBps = bps;
    }

    function currentSnipeTaxBps(address) external view returns (uint256) {
        return taxBps;
    }

    function token() external view returns (address) {
        return address(launched);
    }

    /// The other half of the curve: pulls the tokens, pays out the quote.
    function sell(uint256 tokensIn, uint256 minQuoteOut, address recipient)
        external
        returns (uint256 quoteOut)
    {
        uint256 before = launched.balanceOf(address(this));
        _safeCall(address(launched), abi.encodeWithSelector(0x23b872dd, msg.sender, address(this), tokensIn));
        uint256 received = launched.balanceOf(address(this)) - before;
        require(received != 0, "zero");

        quoteOut = (quoteReserve * received) / (tokenReserve + received);
        if (quoteOut < minQuoteOut) revert SlippageExceeded(quoteOut, minQuoteOut);
        quoteReserve -= quoteOut;
        tokenReserve += received;

        if (pairToken == address(0)) {
            (bool ok,) = recipient.call{value: quoteOut}("");
            require(ok, "native");
        } else {
            _safeCall(pairToken, abi.encodeWithSelector(0xa9059cbb, recipient, quoteOut));
        }
    }

    function _safeCall(address token, bytes memory data) internal {
        (bool ok, bytes memory returned) = token.call(data);
        require(ok && (returned.length == 0 || abi.decode(returned, (bool))), "safe call");
    }

    function buy(uint256 quoteIn, uint256 minTokensOut, address recipient)
        external
        payable
        returns (uint256 tokensOut)
    {
        uint256 received;
        if (pairToken == address(0)) {
            require(msg.value == quoteIn, "value");
            received = quoteIn;
        } else {
            require(msg.value == 0, "value");
            uint256 before = MockToken(pairToken).balanceOf(address(this));
            // Low level, the way SafeERC20 does it in the real curve: a token
            // that returns nothing is not a token that failed.
            _safeCall(pairToken, abi.encodeWithSelector(0x23b872dd, msg.sender, address(this), quoteIn));
            received = MockToken(pairToken).balanceOf(address(this)) - before;
        }
        require(received != 0, "zero");

        uint256 spent = received;
        uint256 net = spent - (spent * taxBps) / 10_000;
        tokensOut = (net * tokenReserve) / (quoteReserve + net);

        uint256 sellable = tokenReserve - reservedTokens;
        if (tokensOut > sellable) {
            tokensOut = sellable;
            uint256 needed = (sellable * quoteReserve) / (tokenReserve - sellable) + 1;
            uint256 gross = (needed * 10_000) / (10_000 - taxBps) + 1;
            spent = gross < received ? gross : received;
        }
        if (spent * minTokensOut > received * tokensOut) {
            revert SlippageExceeded(tokensOut, minTokensOut);
        }

        quoteReserve += spent;
        tokenReserve -= tokensOut;
        launched.transfer(recipient, tokensOut);

        uint256 refund = received - spent;
        if (refund != 0) {
            if (pairToken == address(0)) {
                (bool sent,) = payable(msg.sender).call{value: refund}("");
                require(sent, "refund");
            } else {
                _safeCall(pairToken, abi.encodeWithSelector(0xa9059cbb, msg.sender, refund));
            }
        }
    }
}

/// A quote asset that calls back into the sniper mid-transfer.
contract ReentrantToken is MockToken {
    address public target;
    bytes public payload;

    constructor() MockToken(0, false, false) {}

    function arm(address target_, bytes calldata payload_) external {
        target = target_;
        payload = payload_;
    }

    function transferFrom(address from, address to, uint256 amount) external override returns (bool) {
        if (target != address(0)) {
            (bool ok, bytes memory err) = target.call(payload);
            // Bubble the revert up so the test can see which one it was.
            if (!ok) {
                assembly {
                    revert(add(err, 32), mload(err))
                }
            }
        }
        uint256 allowed = allowance[from][msg.sender];
        require(allowed >= amount, "allowance");
        if (allowed != type(uint256).max) allowance[from][msg.sender] = allowed - amount;
        _move(from, to, amount);
        return true;
    }
}
