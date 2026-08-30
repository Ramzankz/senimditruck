//! Dala-Trans Escrow — demo смарт-контракт (Solana native program)
//!
//! МАҚСАТЫ: жүк тасымалдау төлемдерін "шартты сақтау" (escrow) арқылы қорғау.
//! Жүк иесі ақшаны келісілген сомаға "құлыптайды", тек жеткізу расталғанда
//! ғана айдаушыға шығады. Дау туындаса, тағайындалған төреші (arbiter)
//! шешім қабылдайды.
//!
//! ЕСКЕРТУ: бұл — концепция дәлелі (proof of concept) деңгейіндегі демо.
//! Production-да (нақты ақшамен) қолданбас бұрын міндетті түрде тәуелсіз
//! қауіпсіздік аудитінен өту керек.

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
    system_instruction,
    msg,
};
use borsh::{BorshDeserialize, BorshSerialize};

fn try_from_slice_unchecked<T: BorshDeserialize>(data: &[u8]) -> Result<T, ProgramError> {
    T::try_from_slice(data).map_err(|_| ProgramError::InvalidAccountData)
}

entrypoint!(process_instruction);

/// Escrow-тың мүмкін болатын күйлері
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub enum EscrowStatus {
    /// Жүк иесі ақшаны құлыптады, жеткізу күтілуде
    Locked,
    /// Жеткізу расталды, ақша айдаушыға шықты
    Released,
    /// Дау туындады, төрешінің шешімі күтілуде
    Disputed,
    /// Жүк иесіне қайтарылды (мысалы, мерзім өтті немесе төреші солай шешті)
    Refunded,
}

/// Escrow есептік деректерінің құрылымы (on-chain сақталады)
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct EscrowAccount {
    /// Осы аккаунт инициализацияланғанын білдіретін белгі
    pub is_initialized: bool,
    /// Жүк иесінің (төлеуші) әмиян адресі
    pub cargo_owner: Pubkey,
    /// Айдаушының (алушы) әмиян адресі
    pub driver: Pubkey,
    /// Дау туындағанда шешім қабылдайтын төреші (мыс. Dala-Trans платформасы)
    pub arbiter: Pubkey,
    /// Құлыпталған сома (lamports)
    pub amount: u64,
    /// Ағымдағы күй
    pub status: EscrowStatus,
    /// Жүк/тапсырыс идентификаторы (сыртқы жүйемен байланыстыру үшін)
    pub order_id: [u8; 32],
}

impl EscrowAccount {
    pub const LEN: usize = 1 + 32 + 32 + 32 + 8 + 1 + 32;
}

/// Контрактқа жіберілетін нұсқаулар (instructions)
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum EscrowInstruction {
    /// 0. Жүк иесі жаңа escrow ашады және сомманы құлыптайды.
    ///    Accounts: [signer: cargo_owner, writable: escrow_account, driver, arbiter, system_program]
    InitializeAndDeposit { amount: u64, order_id: [u8; 32] },

    /// 1. Жеткізу расталды — ақша айдаушыға шығады.
    ///    Бұл әрекетті ЖҮК ИЕСІ немесе QR-код растауын өңдейтін
    ///    сенімді backend (арнайы pubkey) ғана шақыра алады.
    ///    Accounts: [signer: confirmer, writable: escrow_account, writable: driver]
    ConfirmDelivery,

    /// 2. Дау ашу — екі тараптың бірі шақыра алады.
    ///    Accounts: [signer: disputer, writable: escrow_account]
    RaiseDispute,

    /// 3. Төреші дауды шешеді: ақшаны not айдаушыға, not жүк иесіне қайтарады.
    ///    Accounts: [signer: arbiter, writable: escrow_account, writable: driver, writable: cargo_owner]
    ResolveDispute { release_to_driver: bool },
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let instruction = EscrowInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    match instruction {
        EscrowInstruction::InitializeAndDeposit { amount, order_id } => {
            initialize_and_deposit(program_id, accounts, amount, order_id)
        }
        EscrowInstruction::ConfirmDelivery => confirm_delivery(program_id, accounts),
        EscrowInstruction::RaiseDispute => raise_dispute(program_id, accounts),
        EscrowInstruction::ResolveDispute { release_to_driver } => {
            resolve_dispute(program_id, accounts, release_to_driver)
        }
    }
}

fn initialize_and_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
    order_id: [u8; 32],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let cargo_owner = next_account_info(account_info_iter)?;
    let escrow_account = next_account_info(account_info_iter)?;
    let driver = next_account_info(account_info_iter)?;
    let arbiter = next_account_info(account_info_iter)?;
    let system_program = next_account_info(account_info_iter)?;

    if !cargo_owner.is_signer {
        msg!("Қате: жүк иесі транзакцияға қол қоюы керек");
        return Err(ProgramError::MissingRequiredSignature);
    }

    if amount == 0 {
        msg!("Қате: сома 0-ден көп болуы керек");
        return Err(ProgramError::InvalidArgument);
    }

    if escrow_account.owner != program_id {
        msg!("Қате: escrow аккаунты алдын ала осы контракт арқылы жасалуы керек");
        return Err(ProgramError::IncorrectProgramId);
    }

    // Ақшаны жүк иесінен escrow аккаунтқа аудару (құлыптау)
    invoke(
        &system_instruction::transfer(cargo_owner.key, escrow_account.key, amount),
        &[
            cargo_owner.clone(),
            escrow_account.clone(),
            system_program.clone(),
        ],
    )?;

    let escrow_data = EscrowAccount {
        is_initialized: true,
        cargo_owner: *cargo_owner.key,
        driver: *driver.key,
        arbiter: *arbiter.key,
        amount,
        status: EscrowStatus::Locked,
        order_id,
    };

    escrow_data.serialize(&mut &mut escrow_account.data.borrow_mut()[..])?;

    msg!(
        "Escrow ашылды. Сома құлыпталды: {} lamports, тапсырыс: {:?}",
        amount,
        order_id
    );

    Ok(())
}

fn confirm_delivery(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let confirmer = next_account_info(account_info_iter)?;
    let escrow_account = next_account_info(account_info_iter)?;
    let driver = next_account_info(account_info_iter)?;

    if !confirmer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    if escrow_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let mut escrow_data =
        try_from_slice_unchecked::<EscrowAccount>(&escrow_account.data.borrow())?;

    if !escrow_data.is_initialized {
        msg!("Қате: escrow инициализацияланбаған");
        return Err(ProgramError::UninitializedAccount);
    }

    if escrow_data.status != EscrowStatus::Locked {
        msg!("Қате: бұл escrow тек 'Locked' күйінде ғана расталуы мүмкін");
        return Err(ProgramError::InvalidAccountData);
    }

    // Тек жүк иесі немесе тіркелген айдаушының өзі растай алады
    // (нақты жүйеде мұның орнына сенімді "confirmer" pubkey — мыс. QR
    // растауын өңдейтін backend қызметінің кілті болуы мүмкін)
    if confirmer.key != &escrow_data.cargo_owner && confirmer.key != &escrow_data.driver {
        msg!("Қате: бұл әрекетке рұқсатыңыз жоқ");
        return Err(ProgramError::InvalidAccountData);
    }

    if driver.key != &escrow_data.driver {
        msg!("Қате: driver аккаунты escrow-да тіркелген айдаушыға сай келмейді");
        return Err(ProgramError::InvalidArgument);
    }

    let amount = escrow_data.amount;

    **escrow_account.try_borrow_mut_lamports()? -= amount;
    **driver.try_borrow_mut_lamports()? += amount;

    escrow_data.status = EscrowStatus::Released;
    escrow_data.serialize(&mut &mut escrow_account.data.borrow_mut()[..])?;

    msg!("Жеткізу расталды. {} lamports айдаушыға шықты.", amount);

    Ok(())
}

fn raise_dispute(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let disputer = next_account_info(account_info_iter)?;
    let escrow_account = next_account_info(account_info_iter)?;

    if !disputer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    if escrow_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let mut escrow_data =
        try_from_slice_unchecked::<EscrowAccount>(&escrow_account.data.borrow())?;

    if escrow_data.status != EscrowStatus::Locked {
        msg!("Қате: тек 'Locked' күйіндегі escrow-ға дау ашуға болады");
        return Err(ProgramError::InvalidAccountData);
    }

    if disputer.key != &escrow_data.cargo_owner && disputer.key != &escrow_data.driver {
        msg!("Қате: тек жүк иесі немесе айдаушы дау аша алады");
        return Err(ProgramError::InvalidAccountData);
    }

    escrow_data.status = EscrowStatus::Disputed;
    escrow_data.serialize(&mut &mut escrow_account.data.borrow_mut()[..])?;

    msg!("Дау ашылды. Төрешінің шешімі күтілуде.");

    Ok(())
}

fn resolve_dispute(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    release_to_driver: bool,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let arbiter = next_account_info(account_info_iter)?;
    let escrow_account = next_account_info(account_info_iter)?;
    let driver = next_account_info(account_info_iter)?;
    let cargo_owner = next_account_info(account_info_iter)?;

    if !arbiter.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    if escrow_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    let mut escrow_data =
        try_from_slice_unchecked::<EscrowAccount>(&escrow_account.data.borrow())?;

    if escrow_data.status != EscrowStatus::Disputed {
        msg!("Қате: тек 'Disputed' күйіндегі escrow шешілуі мүмкін");
        return Err(ProgramError::InvalidAccountData);
    }

    if arbiter.key != &escrow_data.arbiter {
        msg!("Қате: тек тағайындалған төреші дауды шеше алады");
        return Err(ProgramError::InvalidAccountData);
    }

    let amount = escrow_data.amount;

    if release_to_driver {
        if driver.key != &escrow_data.driver {
            return Err(ProgramError::InvalidArgument);
        }
        **escrow_account.try_borrow_mut_lamports()? -= amount;
        **driver.try_borrow_mut_lamports()? += amount;
        escrow_data.status = EscrowStatus::Released;
        msg!("Төреші шешімі: {} lamports айдаушыға шықты.", amount);
    } else {
        if cargo_owner.key != &escrow_data.cargo_owner {
            return Err(ProgramError::InvalidArgument);
        }
        **escrow_account.try_borrow_mut_lamports()? -= amount;
        **cargo_owner.try_borrow_mut_lamports()? += amount;
        escrow_data.status = EscrowStatus::Refunded;
        msg!("Төреші шешімі: {} lamports жүк иесіне қайтарылды.", amount);
    }

    escrow_data.serialize(&mut &mut escrow_account.data.borrow_mut()[..])?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escrow_account_serialization_roundtrip() {
        let original = EscrowAccount {
            is_initialized: true,
            cargo_owner: Pubkey::new_unique(),
            driver: Pubkey::new_unique(),
            arbiter: Pubkey::new_unique(),
            amount: 1_500_000,
            status: EscrowStatus::Locked,
            order_id: [7u8; 32],
        };

        let mut buf = Vec::new();
        original.serialize(&mut buf).expect("serialize should succeed");

        let decoded = EscrowAccount::try_from_slice(&buf).expect("deserialize should succeed");

        assert_eq!(decoded.is_initialized, original.is_initialized);
        assert_eq!(decoded.cargo_owner, original.cargo_owner);
        assert_eq!(decoded.driver, original.driver);
        assert_eq!(decoded.arbiter, original.arbiter);
        assert_eq!(decoded.amount, original.amount);
        assert_eq!(decoded.status, original.status);
        assert_eq!(decoded.order_id, original.order_id);
    }

    #[test]
    fn escrow_account_len_matches_serialized_size() {
        let account = EscrowAccount {
            is_initialized: true,
            cargo_owner: Pubkey::new_unique(),
            driver: Pubkey::new_unique(),
            arbiter: Pubkey::new_unique(),
            amount: 42,
            status: EscrowStatus::Disputed,
            order_id: [1u8; 32],
        };

        let mut buf = Vec::new();
        account.serialize(&mut buf).expect("serialize should succeed");

        // Тек негізгі мемлекеттерге сай екенін тексереміз — EscrowStatus enum
        // 1 байт алады (u8 discriminant), сондықтан LEN мұны да қосуы керек.
        assert_eq!(buf.len(), EscrowAccount::LEN);
    }

    #[test]
    fn status_transitions_are_distinct() {
        assert_ne!(EscrowStatus::Locked, EscrowStatus::Released);
        assert_ne!(EscrowStatus::Locked, EscrowStatus::Disputed);
        assert_ne!(EscrowStatus::Disputed, EscrowStatus::Released);
        assert_ne!(EscrowStatus::Disputed, EscrowStatus::Refunded);
    }
}
