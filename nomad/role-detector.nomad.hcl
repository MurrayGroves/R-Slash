job "role-detector" {
    type = "service"

    vault {
        # Don't need to access Vault inside of job
        env = false
        policies = ["role-detector"]
    }

    group "role-detector" {
        count = 1

        task "detector" {
            driver = "docker"
            config = {
                image = "registry.murraygrov.es/discord-roles"
            }

            env = {
                RUST_LOG = "role_detector=info"
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
    }
}