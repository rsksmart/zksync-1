import { setupSimulation } from './simulations/setup';
import { runTransferToNewSimulation } from './simulations/transferToNew';
import { runDepositSimulation } from './simulations/deposit';
import config from './utils/config.utils';
import { runChangePubKeySimulation } from './simulations/changePubKey';

// FIXME: this is a workaround for the fact that the simulation tool is not yet ready to run multiple simulations in parallel
(async function () {
    const tasks = [runDepositSimulation, runTransferToNewSimulation, runChangePubKeySimulation];

    config.totalRunningTimeSeconds = Math.floor(config.totalRunningTimeSeconds / tasks.length); // FIXME: we shouldn't have more than one config state so mutating config is not a good idea
    const simulationSetup = await setupSimulation();

    for (const task of tasks) {
        await task(simulationSetup);
    }
})();
