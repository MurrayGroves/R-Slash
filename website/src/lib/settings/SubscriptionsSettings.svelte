<script lang="ts">
	import type { Backend, Channel } from '$lib/shared';
	import { onMount } from 'svelte';

	import { Grid, PrelineTheme } from '@mediakular/gridcraft';

	let { backend, channel }: { backend: Backend, channel: Channel } = $props();

	interface SubscriptionUI {
		subreddit: string;
		added_at: string;
	}

	let subscriptions: SubscriptionUI[] | null = $state(null);

	onMount(async () => {
		let subscriptions_resp = await backend.getChannelSubscriptions(channel.id);
		subscriptions = subscriptions_resp.map(subs => {
			return {
				subreddit: 'r/' + subs.subreddit,
				added_at: new Date(subs.added_at * 1000).toLocaleString(undefined, {
					year: 'numeric',
					month: '2-digit',
					day: '2-digit',
					hour: '2-digit',
					minute: '2-digit'
				})
			};
		});
	});
</script>

<div class="mt-5 border-gray-700 border-s bg-slate-800 rounded-s p-2">
	<h2 class="text-2xl mb-2 font-semibold">Subscriptions</h2>
	{#if subscriptions === null}
		<p>Loading...</p>
	{:else}
		<Grid bind:data={subscriptions} theme={PrelineTheme} columns={[
			{ title: 'Subreddit', key: 'subreddit'},
			{ title: 'Added At', key: 'added_at'}
		]} />
	{/if}
</div>