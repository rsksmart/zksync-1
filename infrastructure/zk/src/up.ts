import { Command } from 'commander';
import * as utils from './utils';

export async function up() {
    await utils.spawn('docker-compose up -d postgres rskj dev-ticker');
    await utils.spawn('docker-compose up -d tesseracts');
}

export const command = new Command('up').description('start development containers').action(up);
