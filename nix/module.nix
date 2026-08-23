self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.auto-commit;
  format = pkgs.formats.toml {};

  leaks = option: path: {
    assertion = path == null || !lib.hasPrefix builtins.storeDir (toString path);
    message = ''
      programs.auto-commit.${option} points into the nix store, which is world
      readable, so its contents would be published to every user on this
      machine. Give it a path produced at activation time, such as
      config.sops.secrets.<name>.path, rather than a path literal.
    '';
  };

  # The named options win over anything in `settings`, so a typo in `settings`
  # can never quietly shadow a real option.
  named = lib.filterAttrs (_: value: value != null) {
    model = cfg.model;
    endpoint = cfg.endpoint.value;
    endpoint_file = lib.mapNullable toString cfg.endpoint.file;
    api_key_file = lib.mapNullable toString cfg.apiKey.file;
  };

  merged = cfg.settings // named;
in {
  options.programs.auto-commit = {
    enable = lib.mkEnableOption "auto-commit, a commit message writer";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.auto-commit;
      defaultText = lib.literalExpression "auto-commit.packages.\${system}.auto-commit";
      description = "The auto-commit package to install.";
    };

    model = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "qwen2.5-coder:7b";
      description = ''
        Model to ask for, named exactly as the endpoint expects it. Leave it
        unset to use the tool's own default.
      '';
    };

    apiKey = lib.mkOption {
      default = {};
      description = "Where auto-commit reads the API key from.";
      type = lib.types.submodule {
        options.file = lib.mkOption {
          type = lib.types.nullOr lib.types.path;
          default = null;
          example = lib.literalExpression "config.sops.secrets.auto-commit-api-key.path";
          description = ''
            Path to a file holding the API key, opened by auto-commit at run
            time. Only the path is written to the generated config, so the key
            never enters the nix store.

            There is deliberately no option to write the key inline: anything
            set in a NixOS configuration ends up in the store.

            An endpoint that wants no key can be given any placeholder through
            {env}`AUTO_COMMIT_API_KEY` instead.
          '';
        };
      };
    };

    endpoint = lib.mkOption {
      default = {};
      description = "Which API auto-commit talks to, and where that comes from.";
      type = lib.types.submodule {
        options = {
          value = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "http://127.0.0.1:11434/v1";
            description = ''
              Base URL of the API. Anything speaking the OpenAI wire format
              works: a hosted provider, a gateway, a local `llama.cpp` server,
              or Ollama on `http://127.0.0.1:11434/v1`.

              Use {option}`programs.auto-commit.endpoint.file` instead when the
              URL is itself a secret, which it is whenever it carries a tenant
              or a token.
            '';
          };

          file = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            example = lib.literalExpression "config.sops.secrets.auto-commit-endpoint.path";
            description = ''
              Path to a file holding the base URL, opened by auto-commit at
              run time. Only the path is written to the generated config, so
              the URL never enters the nix store.
            '';
          };
        };
      };
    };

    settings = lib.mkOption {
      type = format.type;
      default = {};
      example = {
        context_commits = 10;
        conventional_commits = true;
        max_tool_calls = 2;
        exclude = ["*.lock" "dist/**"];
      };
      description = ''
        Anything else from the auto-commit config file, written verbatim to
        {file}`/etc/auto-commit/config.toml`. Run
        {command}`auto-commit config get` to see every key and its current
        value.

        This is the lowest layer. A user's own
        {file}`~/.config/auto-commit/config.toml`, a repository's
        {file}`.auto-commit.toml`, the environment, and command line flags all
        override it, in that order.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      (leaks "apiKey.file" cfg.apiKey.file)
      (leaks "endpoint.file" cfg.endpoint.file)
      {
        assertion = cfg.endpoint.value == null || cfg.endpoint.file == null;
        message = ''
          programs.auto-commit.endpoint has both `value` and `file` set. Pick
          one: `value` for a URL that can sit in the nix store, `file` for one
          that cannot.
        '';
      }
    ];

    environment.systemPackages = [cfg.package];

    environment.etc."auto-commit/config.toml" = lib.mkIf (merged != {}) {
      source = format.generate "auto-commit-config.toml" merged;
    };
  };
}
