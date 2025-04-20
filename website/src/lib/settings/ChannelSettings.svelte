<script lang="ts">
	import SubscriptionsSettings from '$lib/settings/SubscriptionsSettings.svelte';
	import type { Backend, Channel } from '$lib/shared';

	let { backend, channel }: { backend: Backend, channel: Channel } = $props();

</script>

<SubscriptionsSettings backend={backend} channel={channel} />