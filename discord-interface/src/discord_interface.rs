#[tarpc::service]
pub trait DiscordInterface {
    async fn request_boot(shard_id: usize) -> bool;

    async fn finished_boot(shard_id: usize);
}
