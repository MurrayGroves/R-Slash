job "[[ .bot_name ]]" {
    type = "service"

    vault {
        # Don't need to request from Vault inside of the job
        env = false
        policies = ["[[ .bot_name ]]"]
    }

    group "shards" {
        count = [[ .shard.shard_count ]]

        task "shard" {
            driver = "docker"
            config = {
                image = "registry.murraygrov.es/discord-shard:[[ .shard.version ]]"
            }

            env = {
                RUST_LOG = "discord_shard=debug"
            }

            # Load secrets from Vault into environment variables
            template {
                destination = "secrets/file.env"
                destination = "secrets/file.env"
                data = <<EOH
                DISCORD_TOKEN = "{{with secret "secret/data/external/[[ .bot_name ]]/discord"}}{{.Data.data.discord_token}}{{end}}"
                DISCORD_APPLICATION_ID = "{{with secret "secret/data/external/[[ .bot_name ]]/discord"}}{{.Data.data.discord_application_id}}{{end}}"
                POSTHOG_API_KEY = "{{with secret "secret/data/external/posthog"}}{{.Data.data.posthog_api_key}}{{end}}"
                EOH
            }
        }

        update {
            max_parallel = [[ .shard.max_concurrency ]]

            min_healthy_time = "5s"
            healthy_deadline = "1m"
            # Mark deployment as failed if any of the allocations don't become healthy
            progress_deadline = "0"
            auto_revert = true
        }
    }

    group "discord-interface" {
        count = 1

        task "interface" {
            driver = "docker"
            config = {
                image = "registry.murraygrov.es/discord-interface:[[ .interface.version ]]"
            }

            env = {
                RUST_LOG = "discord_interface=info"
            }

            # Load secrets from Vault into environment variables
            template {
                destination = "secrets/file.env"
                data = <<EOH
                DISCORD_TOKEN = "{{with secret "secret/data/external/[[ .bot_name ]]/discord"}}{{.Data.data.discord_token}}{{end}}"
                DISCORD_APPLICATION_ID = "{{with secret "secret/data/external/[[ .bot_name ]]/discord"}}{{.Data.data.discord_application_id}}{{end}}"
                EOH
            }
        }
    }
}