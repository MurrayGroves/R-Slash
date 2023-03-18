job "redis" {
    type = "service"

    group "redis" {
        count = 1

        ephemeral_disk {
            size = 300
        }

        task "redis" {
            driver = "docker"
            config {
                image = "redis/redis-stack-server"
                ports = ["redis"]
            }

            resources {
                network {
                    # Ensure it's placed on a node with that port available
                    port "redis" {
                        static = 6379
                    }
                }
            }

            service {
                name = "redis"
                tags = ["global", "cache", "urlprefix-/redis" ]
                port = "redis"
                check {
                name     = "alive"
                type     = "tcp"
                interval = "10s"
                timeout  = "2s"
                }
            }
        }
    }
}