job "consul" {
    type = "system"

    group "consul" {
        count = 1

        task "consul" {
            driver = "docker"

            config {
                image = "consul"
                network_mode = "host"
                port_map {
                    http = 8600
                }
            }

            service {
                name = "consul"
                port = "http"
                tags = ["global"]
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
                source = "consul-data-mediaserver"

                access_mode = "single-node-writer"
                attachment_mode = "file-system"
            }
        }
    }
}