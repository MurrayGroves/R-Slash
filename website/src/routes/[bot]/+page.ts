import { Bot } from '$lib/types';

export async function load({params}) {
	return {
		bot: params.bot == 'bb' ? Bot.BB : Bot.RS
	}
}