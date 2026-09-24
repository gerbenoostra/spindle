# Non-flake fallback. The flake and this import the same derivation.
{
  pkgs ? import <nixpkgs> { },
}:

pkgs.callPackage ./nix/package.nix { }
