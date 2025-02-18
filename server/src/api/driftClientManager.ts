import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { Wallet, DriftClient, User as DriftUser, calculateDepositRate, calculateBorrowRate } from "@drift-labs/sdk";
import { RPC_URL } from "../config.js";
import { bnToDecimal } from "../helpers.js";

export async function getDriftData(address: string, marketIndices: number[], driftClientManager: DriftClientManager) {
    const balancePromise = driftClientManager.getUserBalances(address, marketIndices);

    // TODO - Get withdraw limit with Quartz health, not Drift health
    const getWithdrawLimitPromise = async () => {
        const promises = marketIndices.map(async (index) => {
            const withdrawalLimit = await driftClientManager.getWithdrawalLimit(address, index);
            if (withdrawalLimit === null) throw new Error(`Could not find withdrawal limit for market index ${index}`);
        
            return withdrawalLimit;
        });
        return await Promise.all(promises);
    }

    const getRatePromise = async() => {
        const promises = marketIndices.map(async (index) => {
            const spotMarket = await driftClientManager.getSpotMarketAccount(index);
            if (!spotMarket) throw new Error(`Could not find spot market for index ${index}`);
        
            const depositRateBN = calculateDepositRate(spotMarket);
            const borrowRateBN = calculateBorrowRate(spotMarket);
        
            return {
                depositRate: bnToDecimal(depositRateBN, 6),
                borrowRate: bnToDecimal(borrowRateBN, 6)
            };
        });
        return await Promise.all(promises);
    };

    const healthPromise = driftClientManager.getUserHealth(address);

    const [balances, withdrawLimits, rates, health] = await Promise.all([
        balancePromise, getWithdrawLimitPromise(), getRatePromise(), healthPromise
    ]);
    
    return {
        balances,
        withdrawLimits,
        rates,
        health
    }
}


export async function getDriftApy(driftClientManager: DriftClientManager) {
    const USDC_MARKET_INDEX = 0;

    const spotMarket = await driftClientManager.getSpotMarketAccount(USDC_MARKET_INDEX);
    if (!spotMarket) throw new Error(`Could not find spot market for index ${USDC_MARKET_INDEX}`);

    const depositRateBN = calculateDepositRate(spotMarket);
    const apy = bnToDecimal(depositRateBN, 6);
    return apy;
}
  

export class DriftClientManager {
    private driftClient!: DriftClient;
    private connection!: Connection;
    private wallet!: Wallet;
    private reconnectAttempts: number = 0;
    private maxReconnectAttempts: number = 10;
    private baseReconnectDelay: number = 1000; // 1 second

    constructor() {
        this.initializeDriftClient();
    }

    private async initializeDriftClient() {
        try {
            this.connection = new Connection(RPC_URL);
 
            this.wallet = new Wallet(Keypair.generate());
            console.log("Wallet created with keypair:", this.wallet.publicKey.toBase58());

            this.driftClient = new DriftClient({
                connection: this.connection,
                wallet: this.wallet,
                env: 'mainnet-beta',
            });

            await this.driftClient.subscribe();
            console.log('DriftClient initialized and subscribed successfully');
            this.reconnectAttempts = 0; // Reset reconnect attempts on successful connection
        } catch (error) {
            console.error('Error initializing DriftClient:', error);
            this.handleReconnection();
        }
    }

    private async handleReconnection() {
        if (this.reconnectAttempts < this.maxReconnectAttempts) {
            const delay = this.baseReconnectDelay * Math.pow(2, this.reconnectAttempts);
            console.log(`Attempting to reconnect in ${delay}ms...`);
            setTimeout(() => {
                this.reconnectAttempts++;
                this.initializeDriftClient();
            }, delay);
        } else {
            console.error('Max reconnection attempts reached. Please check your connection and try again later.');
        }
    }

    public async getUserBalances(address: string, marketIndices: number[]): Promise<any> {
        await this.emulateAccount(new PublicKey(address));
        const user = this.getUser();

        const balances = marketIndices.map((index) => {
            return Number(user.getTokenAmount(index));
        });

        return balances;
    }

    public async getUserHealth(address: string) {
        await this.emulateAccount(new PublicKey(address));
        const user = this.getUser();
        return user.getHealth();
    }

    public async getSpotMarketAccount(marketIndex: number) {
        return await this.driftClient.getSpotMarketAccount(marketIndex);
    }

    public async getWithdrawalLimit(address: string, marketIndex: number) {
        await this.emulateAccount(new PublicKey(address));
        const user = this.getUser();
        return user.getWithdrawalLimit(marketIndex, false).toNumber();
    }

    getUser(): DriftUser {
        return this.driftClient.getUser();
    }

    async emulateAccount(address: PublicKey) {
        await this.driftClient.emulateAccount(address);
    }
}
