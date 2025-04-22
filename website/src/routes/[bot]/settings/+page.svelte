<script lang="ts">
	import { Button } from 'flowbite-svelte';
	import { Backend, type Channel, getConfig, type Guild, TypeOfSelection } from '$lib/shared';
	import { onMount } from 'svelte';

	import { ChevronDown, ChevronRight } from '@lucide/svelte';
	import ChannelSettings from '$lib/settings/ChannelSettings.svelte';
	import { prettyPrintBot } from '$lib/types.js';

	let { data } = $props();
	let config = getConfig(data.bot);

	let backend: Backend | null = $state(null);
	let guilds: UIGuild[] = $state([]);
	let channels: { [Key: string]: Channel[] } = $state({});

	let selection: [TypeOfSelection, Guild | Channel | null] = $state([TypeOfSelection.None, null]);

	interface UIGuild extends Guild {
		hover?: boolean;
		expanded?: boolean;
	}

	onMount(async () => {
		backend = await Backend.create(data.bot);
		guilds = await backend.getGuilds();
		guilds = guilds.filter((guild) => (guild.permissions & 0x0000000000000020) === 0x0000000000000020);
		console.log(guilds);
		channels = await backend.getGuildsChannels(guilds);
		console.log(channels);

		for (const [key, value] of Object.entries(channels)) {
			let roots: Channel[] = value.filter(channel => channel.type === 4 || channel.parent_id === null);

			let newOrder: Channel[] = [];
			roots.sort((a, b) => a.position !== b.position ? a.position - b.position : Number.parseInt(a.id) - Number.parseInt(b.id));
			for (let root of roots) {
				if (root.type !== 4) {
					newOrder.push(root);
					continue;
				}

				let categoryChannels = value.filter(channel => channel.parent_id === root.id && channel.type === 0);
				if (categoryChannels.length > 0) {
					newOrder.push(root);
					categoryChannels.sort((a, b) => a.position !== b.position ? a.position - b.position : Number.parseInt(a.id) - Number.parseInt(b.id));
					newOrder = newOrder.concat(categoryChannels);
				}
			}

			channels[key] = newOrder;
		}
		console.log(channels);

	});
</script>

<div class="bg-slate-850 min-h-screen text-gray-200 flex-col flex">
	<div class="bg-slate-900 min-h-20 flex-initial">
		<div class="flex w-full">
			<enhanced:img src={config.botProfile} class="size-[4rem] rounded-full aspect-square m-3 mx-3 md:mx-5 flex-1"
										alt="Bot Logo" />
			<h1 class="my-auto text-2xl md:text-4xl flex-1">
				{prettyPrintBot(data.bot)} Settings
			</h1>
			<Button class="self-end flex-10 md:mr-5 my-auto bg-[#5865F2] p-2.5 rounded-md min-w-32 font-bold" on:click={async () => {
				await config.userManager.removeUser()
				window.location.href = `/${config.botShorthand}`
			}}>
				Log out
			</Button>
		</div>
	</div>

	<div class="flex w-full min-h-full flex-grow">
		<div class="flex-1 flex-shrink bg-slate-800 p-5">
			<span class="text-2xl font-bold">Servers</span>
			<hr class="mt-1 mb-1">
			<div role="tree">
				{#each guilds as guild}
					<button class="flex flex-row min-h-8" onmouseenter={() => {guild.hover = true}}
									onmouseleave={() => {guild.hover = false}}
									onclick={() => {guild.expanded = !guild.expanded; selection = [TypeOfSelection.Guild, guild];}}
									role="treeitem" aria-selected={guild === selection[1]}>
						{#if guild.expanded}
							<ChevronDown size={guild.hover ? 32 : 24} />
						{:else}
							<ChevronRight size={guild.hover ? 32 : 24} />
						{/if}
						<span class="mt-auto mb-auto">{`${guild.name}`}</span>
					</button>
					{#if guild.expanded}
						{#each channels[guild.id] as channel}
							<div class="ml-10">
								{#if channel.type === 4}
									<div class="flex">
										<hr class="flex-1 mt-auto mb-auto flex-grow">
										<span class="flex-auto flex-shrink flex-grow-0 self-center text-lg">{channel.name}</span>
										<hr class="flex-1 mt-auto mb-auto flex-grow">
									</div>
								{:else}
									<button onclick={() => selection = [TypeOfSelection.Channel, channel]}>{channel.name}</button>
									<br>
								{/if}
							</div>
						{/each}
					{/if}
				{/each}
			</div>
		</div>
		<div class="flex-[4] p-5">
			{#if selection[0] !== TypeOfSelection.None}
				<span class="text-2xl font-bold">Settings for {selection[1]?.name}</span>
				<hr class="mt-1">
			{/if}
			{#if selection[0] === TypeOfSelection.Channel && selection[1] && 'type' in selection[1] && backend}
				<ChannelSettings backend={backend} channel={selection[1]} />
			{/if}
		</div>
	</div>
</div>