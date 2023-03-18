job "consul" {
    type = "system"

    group "consul" {
        count = 1

        network {
            port "consul" {
                static = 8600
            }

            port "http" {
                static = 8500
            }
        }

        task "consul" {
            driver = "docker"

            config {
                image = "consul"
                network_mode = "host"
                args = ["bootstrap-expect", "1", "-server", "-ui"]
            }

            volume_mount {
                volume = "data"
                destination = "/consul/data"
                read_only = false
            }
        }

        volume "data" {
            type = "host"
            # Means this deployment can only be deployed on my home server (for now). This will need to be changed to use a proper storage provider 
            source = "consul-data-mediaserver"
        }
    }
}