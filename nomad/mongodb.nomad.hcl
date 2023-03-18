job "mongodb" {
    type = "service"

    group "mongodb" {
        count = 1

        task "mongodb" {
            driver = "docker"

            config {
                image = "mongo"
                ports = ["mongodb"]
            }

            resources {
                network {
                    port "mongodb" {
                        static = 27017
                    }
                }
            }
        }

        service {
            name = "mongodb"
            port = "mongodb"

            check {
                name = "alive"
                type = "tcp"
                interval = "10s"
                timeout = "2s"
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