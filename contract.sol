// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/// @title CustodialPaymentVault
/// @notice Minimal custodial payment processor vault. Customers pay into a per-(token, merchant)
///         balance; the custodian later sweeps funds out on the merchant's behalf. Native ETH is
///         supported alongside ERC20 tokens, keyed internally as token == address(0).
/// @dev No owner/admin/operator role exists on purpose. `sweep`/`sweepAmount` are authorized
///      purely by msg.sender == merchantWallet. Since the backend custodies merchant private
///      keys, it satisfies this by signing the sweep tx directly as the merchant wallet — a role
///      system would be redundant.
contract CustodialPaymentVault is ReentrancyGuard {
    using SafeERC20 for IERC20;

    /// @dev Sentinel token address representing native ETH in `_vault`, `Payment`, and `Swept`.
    address private constant NATIVE = address(0);

    /// token => merchant => balance currently sitting in the vault, unswept
    /// NATIVE (address(0)) is used as the token key for ETH balances.
    mapping(address => mapping(address => uint256)) private _vault;

    error ZeroAddress();
    error ZeroAmount();
    error NothingToSweep();
    error InsufficientBalance();
    error NativeTransferFailed();
    error UseNativePayment();

    /// @dev `identifier` is bytes16 on purpose: a UUIDv4 with dashes stripped is exactly 16
    ///      bytes, so invoice ids from any normal backend/db fit natively with zero re-encoding.
    ///      As an indexed event topic it costs the same as bytes32 (topics are always one 32-byte
    ///      word), but as a calldata argument the 16 padding bytes are zero, and zero bytes cost
    ///      4 gas vs 16 gas for non-zero bytes — so it's strictly cheaper than bytes32 to pass in,
    ///      never more expensive, and it is never written to storage so there's no storage cost
    ///      difference either way.
    event Payment(
        address indexed merchant,
        address indexed token,
        bytes16 indexed identifier,
        address payer,
        uint256 amountRequested,
        uint256 amountReceived,
        uint256 timestamp
    );

    event Swept(
        address indexed merchant,
        address indexed token,
        uint256 amount,
        uint256 timestamp
    );

    /// @notice Pay an invoice with an ERC20 token. Pulls `amount` of `token` from msg.sender and
    ///         credits the merchant's vault with whatever was ACTUALLY received (protects
    ///         accounting against fee-on-transfer / deflationary / non-standard tokens —
    ///         irrelevant for plain USDC today, but this makes the contract safe to reuse as-is
    ///         once you support other tokens/chains).
    /// @param token ERC20 token address being paid with. Must not be address(0); use
    ///        `payNative` for ETH.
    /// @param amount Amount to pull from msg.sender (should match the invoice total, pre-fee).
    /// @param identifier Invoice identifier (see bytes16 note above).
    /// @param merchant Destination merchant wallet — this is your merchant id.
    function pay(
        address token,
        uint256 amount,
        bytes16 identifier,
        address merchant
    ) external nonReentrant {
        if (merchant == address(0)) revert ZeroAddress();
        if (token == NATIVE) revert UseNativePayment();
        if (amount == 0) revert ZeroAmount();

        IERC20 t = IERC20(token);
        uint256 balBefore = t.balanceOf(address(this));
        t.safeTransferFrom(msg.sender, address(this), amount);
        uint256 received = t.balanceOf(address(this)) - balBefore;

        _vault[token][merchant] += received;

        emit Payment(merchant, token, identifier, msg.sender, amount, received, block.timestamp);
    }

    /// @notice Pay an invoice with native ETH. The full msg.value is credited to the merchant's
    ///         vault (no fee-on-transfer concern for native currency, unlike ERC20s).
    /// @param identifier Invoice identifier (see bytes16 note on Payment event).
    /// @param merchant Destination merchant wallet — this is your merchant id.
    function payNative(
        bytes16 identifier,
        address merchant
    ) external payable nonReentrant {
        if (merchant == address(0)) revert ZeroAddress();
        if (msg.value == 0) revert ZeroAmount();

        _vault[NATIVE][merchant] += msg.value;

        emit Payment(merchant, NATIVE, identifier, msg.sender, msg.value, msg.value, block.timestamp);
    }

    /// @notice Sweep the entire vault balance of `token` belonging to msg.sender, to msg.sender.
    ///         Pass address(0) to sweep native ETH.
    /// @dev Effects (zeroing the balance) happen before the external transfer (checks-effects-
    ///      interactions), and nonReentrant blocks any callback-based reentry regardless.
    function sweep(address token) external nonReentrant {
        uint256 amount = _vault[token][msg.sender];
        if (amount == 0) revert NothingToSweep();

        _vault[token][msg.sender] = 0;

        _payout(token, amount);
    }

    /// @notice Sweep exactly `amount` base units of `token` belonging to msg.sender, to
    ///         msg.sender. Pass address(0) for native ETH. Anything not swept stays credited to
    ///         the merchant and can be swept later.
    /// @dev Intended for sweeping only settled/confirmed invoice amounts while leaving the
    ///      remainder (pending invoices, chargeback holds, etc.) in the vault. The vault holds a
    ///      single pooled balance per (token, merchant) — it has no notion of which invoice a
    ///      given wei belongs to — so the backend is responsible for computing `amount` from the
    ///      `Payment` events it considers confirmed.
    /// @dev Same authorization model as `sweep`: destination is hardcoded to msg.sender, so funds
    ///      can only ever leave to the merchant wallet that owns the balance.
    /// @param token Token to sweep. address(0) for native ETH.
    /// @param amount Amount in base units. Must be > 0 and <= the merchant's current balance.
    function sweepAmount(address token, uint256 amount) external nonReentrant {
        if (amount == 0) revert ZeroAmount();

        uint256 balance = _vault[token][msg.sender];
        if (amount > balance) revert InsufficientBalance();

        unchecked {
            // Safe: `amount <= balance` was just checked.
            _vault[token][msg.sender] = balance - amount;
        }

        _payout(token, amount);
    }

    /// @notice Current unswept balance of `token` held for `merchant`. Pass address(0) for ETH.
    function balanceOf(address merchant, address token) external view returns (uint256) {
        return _vault[token][merchant];
    }

    /// @notice Batch read: balances of several tokens for one merchant, one call.
    ///         Include address(0) in `tokens` to read the merchant's ETH balance.
    function balancesOf(address merchant, address[] calldata tokens)
        external
        view
        returns (uint256[] memory balances)
    {
        balances = new uint256[](tokens.length);
        for (uint256 i = 0; i < tokens.length; ++i) {
            balances[i] = _vault[tokens[i]][merchant];
        }
    }

    /// @notice Batch read: balance of one token across several merchants, one call.
    function balancesOfMerchants(address token, address[] calldata merchants)
        external
        view
        returns (uint256[] memory balances)
    {
        balances = new uint256[](merchants.length);
        for (uint256 i = 0; i < merchants.length; ++i) {
            balances[i] = _vault[token][merchants[i]];
        }
    }

    /// @dev Shared payout tail for `sweep` and `sweepAmount`. Callers MUST have already
    ///      decremented `_vault[token][msg.sender]` by `amount` before calling this, so that
    ///      checks-effects-interactions holds; `nonReentrant` on the external entrypoints blocks
    ///      callback-based reentry regardless. Destination is always msg.sender — there is no
    ///      code path in this contract that sends a merchant's balance anywhere else.
    function _payout(address token, uint256 amount) private {
        if (token == NATIVE) {
            (bool success, ) = payable(msg.sender).call{value: amount}("");
            if (!success) revert NativeTransferFailed();
        } else {
            IERC20(token).safeTransfer(msg.sender, amount);
        }

        emit Swept(msg.sender, token, amount, block.timestamp);
    }

    /// @dev Deliberately rejects bare ETH transfers instead of silently crediting them. A plain
    ///      `send`/`transfer`/`call{value:x}("")` to this address carries no merchant or invoice
    ///      identifier, so there is no way to attribute the funds — and since there is no
    ///      owner/admin role, misattributed or stranded ETH could never be recovered. Callers
    ///      must go through `payNative`, which requires an explicit merchant address.
    receive() external payable {
        revert UseNativePayment();
    }
}
