// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

library MathLib { function sq(uint256 x) internal pure returns (uint256) { return x * x; } function cube(uint256 x) external pure returns (uint256) { return x * x * x; } }

contract Token {
    string public name; string public symbol; uint8 public constant decimals = 18;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    error Insufficient(uint256 have, uint256 want);
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    constructor(string memory n, string memory s, uint256 supply) { name = n; symbol = s; totalSupply = supply; balanceOf[msg.sender] = supply; emit Transfer(address(0), msg.sender, supply); }
    function transfer(address to, uint256 v) external returns (bool) { _t(msg.sender, to, v); return true; }
    function approve(address sp, uint256 v) external returns (bool) { allowance[msg.sender][sp] = v; emit Approval(msg.sender, sp, v); return true; }
    function transferFrom(address f, address to, uint256 v) external returns (bool) { allowance[f][msg.sender] -= v; _t(f, to, v); return true; }
    function _t(address f, address to, uint256 v) internal { uint256 b = balanceOf[f]; if (b < v) revert Insufficient(b, v); balanceOf[f] = b - v; balanceOf[to] += v; emit Transfer(f, to, v); }
}

contract Child { address public owner; uint256 public salt; constructor(address o, uint256 s) payable { owner = o; salt = s; } }

contract Factory {
    event Made(address child, bool viaCreate2);
    function make(uint256 s) external payable returns (address c) { c = address(new Child{value: msg.value}(msg.sender, s)); emit Made(c, false); }
    function make2(bytes32 salt, uint256 s) external returns (address c) { c = address(new Child{salt: salt}(msg.sender, s)); emit Made(c, true); }
    function predict(bytes32 salt, uint256 s) external view returns (address) {
        bytes32 h = keccak256(abi.encodePacked(bytes1(0xff), address(this), salt, keccak256(abi.encodePacked(type(Child).creationCode, abi.encode(msg.sender, s)))));
        return address(uint160(uint256(h)));
    }
}

contract Vault {
    mapping(address => uint256) public deposits;
    receive() external payable { deposits[msg.sender] += msg.value; }
    function withdraw(uint256 v) external { deposits[msg.sender] -= v; (bool ok,) = msg.sender.call{value: v}(""); require(ok, "send failed"); }
    function sq(uint256 x) external pure returns (uint256) { return MathLib.sq(x); }
    function cube(uint256 x) external pure returns (uint256) { return MathLib.cube(x); }
    function hashIt(bytes memory d) external pure returns (bytes32, bytes32, address) {
        return (keccak256(d), sha256(d), ecrecover(bytes32(0), 27, bytes32(uint256(1)), bytes32(uint256(2))));
    }
    function env() external view returns (uint256, uint256, uint256, uint256, address, uint256) { return (block.chainid, block.number, block.timestamp, block.basefee, msg.sender, tx.gasprice); }
    function loop(uint256 n) external pure returns (uint256 s) { for (uint256 i; i < n; i++) s += i; }
    function boom() external pure { assert(false); }
    function divz(uint256 a, uint256 b) external pure returns (uint256) { return a / b; }
}

contract Caller {
    function ping(address v, uint256 x) external returns (bool ok, bytes memory r) { (ok, r) = v.call(abi.encodeWithSignature("sq(uint256)", x)); }
    function viaDelegate(address v, uint256 x) external returns (bool ok, bytes memory r) { (ok, r) = v.delegatecall(abi.encodeWithSignature("sq(uint256)", x)); }
    function staticTo(address v) external view returns (bool ok) { (ok,) = v.staticcall(abi.encodeWithSignature("sq(uint256)", 3)); }
}
