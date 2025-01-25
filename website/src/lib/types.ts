export enum Bot {
	BB = 'Booty Bot',
	RS = 'R Slash'
}

export function botShorthand(bot: Bot): string {
	switch (bot) {
		case Bot.BB:
			return 'bb';
		case Bot.RS:
			return 'rs';
	}
}
