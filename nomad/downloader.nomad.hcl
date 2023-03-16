job "reddit-downloader" {
    type = "service"

    vault {
        # Don't need to request from Vault inside of the job
        env = false
        policies = ["downloader"]
    }

    group "global-downloader" {
        count = 1

        task "downloader" {
            driver = "docker"
            config = {
                image = "registry.murraygrov.es/reddit-downloader"
            }

            env {
                DO_CUSTOM = "false"
                RUST_LOG = "debug"
            }

            # Load secrets from Vault into environment variables
            template {
                destination = "secrets/file.env"
                data = <<EOH
                GFYCAT_CLIENT = "{{with secret "secret/data/external/gfycat"}}{{.Data.data.gfycat_client}}{{end}}"
                GFYCAT_SECRET = "{{with secret "secret/data/external/gfycat"}}{{.Data.data.gfycat_secret}}{{end}}"
                REDDIT_CLIENT_ID = "{{with secret "secret/data/external/reddit"}}{{.Data.data.reddit_token}}{{end}}"
                REDDIT_TOKEN = "{{with secret "secret/data/external/reddit"}}{{.Data.data.reddit_token}}{{end}}"
                IMGUR_CLIENT = "{{with secret "secret/data/external/imgur"}}{{.Data.data.imgur_client}}{{end}}"
                IMGUR_SECRET = "{{with secret "secret/data/external/imgur"}}{{.Data.data.imgur_secret}}{{end}}"
                EOH
            }
        }
    }

    group "custom-downloader" {
        count = 1

        task "downloader" {
            driver = "docker"
            config = {
                image = "registry.murraygrov.es/reddit-downloader"
            }

            env {
                DO_CUSTOM = "true"
                RUST_LOG = "debug"
            }

            # Load secrets from Vault into environment variables
            template {
                destination = "secrets/file.env"
                data = <<EOH
                GFYCAT_CLIENT = "{{with secret "secret/data/external/gfycat"}}{{.Data.data.gfycat_client}}{{end}}"
                GFYCAT_SECRET = "{{with secret "secret/data/external/gfycat"}}{{.Data.data.gfycat_secret}}{{end}}"
                REDDIT_CLIENT_ID = "{{with secret "secret/data/external/reddit"}}{{.Data.data.reddit_token}}{{end}}"
                REDDIT_TOKEN = "{{with secret "secret/data/external/reddit"}}{{.Data.data.reddit_token}}{{end}}"
                IMGUR_CLIENT = "{{with secret "secret/data/external/imgur"}}{{.Data.data.imgur_client}}{{end}}"
                IMGUR_SECRET = "{{with secret "secret/data/external/imgur"}}{{.Data.data.imgur_secret}}{{end}}"
                EOH
            }
        }
    }
}