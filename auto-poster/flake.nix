{
  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "nixpkgs/nixos-unstable";
    crane = {
      url = "github:ipetkov/crane";
      inputs = {
        flake-utils.follows = "flake-utils";
        nixpkgs.follows = "nixpkgs";
      };
    };
  };

  outputs = { self, fenix, flake-utils, crane, nixpkgs }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        toolchain = fenix.packages.${system}.minimal.toolchain;
        pkgs = nixpkgs.legacyPackages.${system};
      in

      {
        packages =
          let
            craneLib = (crane.mkLib nixpkgs.legacyPackages.${system}).overrideToolchain toolchain;
          in
          rec {
            default = craneLib.buildPackage {
              # this setup is a little bit scuffed, in an ideal world we would have the flake build the entire cargo project, but that presents a few problems that it's not my job to fix
              # the main one I encountered is that using normal `makeRustPlatform` had some issues because the monorepo has 3 different versions of the `serenity` lib at once, which 
              # was causing conflicts. The workaround, in lieu of actually fixing the inconsistency seems to be to use the `cargoHash` option rather than `cargoLock`, but
              # I don't like that becuase it means _any_ changes to any part of the monorepo's dependencies would require a change to the flake. In the longer term,
              # if nixifying the whole project is the goal, this would be better
              # for now we simply:
              src = ../.; # build the _entire_ repo with crane (so that it knows about the other crates)
              cargoExtraArgs = "-p auto_poster"; # but only actually build this one with cargo
            };
            docker = pkgs.dockerTools.buildLayeredImage {
              name = "auto-poster";
              tag = "latest";
              nativeBuildInputs = [ pkgs.pkg-config ];
              buildInputs = [ pkgs.openssl ];
              config.Entrypoint = [ "${default}/bin/auto_poster" ];
            };
          };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            openssl
          ];
        };

      });
}
