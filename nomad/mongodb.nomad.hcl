job "mongodb" {
    type = "service"

    group "mongodb" {
        count = 1

        task "mongodb" {
            driver = "docker"

            config {
                image = "mongo"
                port_map {
                    mongodb = 27017
                }
            }

            resources {
                network {
                    port "mongodb" {}
                }
            }
        }

        volume "data" {
            type = "host"
            # Means this deployment can only be deployed on my home server (for now). This will need to be changed to use a proper storage provider 
            source = "mongodb-data-mediaserver"

            access_mode = "single-node-writer"
            attachment_mode = "file-system"
        }
    }
}