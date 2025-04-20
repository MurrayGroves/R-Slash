export enum Bot {
	BB = 'bb',
	RS = 'rs'
}

export function botShorthand(bot: Bot): string {
	switch (bot) {
		case Bot.BB:
			return 'bb';
		case Bot.RS:
			return 'rs';
	}
}

export function prettyPrintBot(bot: Bot): string {
	switch (bot) {
		case Bot.BB:
			return 'Booty Bot';
		case Bot.RS:
			return 'R Slash';
	}
}

export function botClientId(bot: Bot): string {
	switch (bot) {
		case Bot.BB:
			return '278550142356029441';
		case Bot.RS:
			return '282921751141285888';
	}
}

export interface ChannelSubscription {
	subreddit: string;
	channel: number;
	bot: Bot;
	added_at: number;
}
