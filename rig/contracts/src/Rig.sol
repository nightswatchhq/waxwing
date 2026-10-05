// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Child {
    event Ping(uint256 n);

    uint256 public n;

    function ping() external {
        n++;
        emit Ping(n);
    }
}

contract Factory {
    event Spawned(address child);

    address[] public children;

    function spawn() external {
        Child child = new Child();
        children.push(address(child));
        emit Spawned(address(child));
    }
}
