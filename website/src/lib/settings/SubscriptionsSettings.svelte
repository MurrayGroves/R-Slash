<script lang="ts">
	import type { Backend, Channel } from '$lib/shared';
	import { onMount } from 'svelte';
	import type { ChannelSubscription } from '$lib/types';

	let { backend, channel }: { backend: Backend, channel: Channel } = $props();


	let subscriptions: ChannelSubscription[] | null = $state(null);

	onMount(async () => {
		console.log('BACKEND', backend);
		subscriptions = await backend.getChannelSubscriptions(channel.id);
	});
</script>

<div>
	{#each subscriptions as subscription}
		<span>{subscription.subreddit}</span>
	{/each}
</div>