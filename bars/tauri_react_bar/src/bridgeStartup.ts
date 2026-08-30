interface BridgeStartupOptions<T> {
  announceReady: () => Promise<void>;
  queryOptionalValue: () => Promise<T>;
  applyOptionalValue: (value: T) => void;
  reportOptionalError: (error: unknown) => void;
  isCancelled: () => boolean;
}

export async function completeBridgeStartup<T>({
  announceReady,
  queryOptionalValue,
  applyOptionalValue,
  reportOptionalError,
  isCancelled,
}: BridgeStartupOptions<T>): Promise<void> {
  await announceReady();
  if (isCancelled()) return;

  try {
    const value = await queryOptionalValue();
    if (!isCancelled()) applyOptionalValue(value);
  } catch (error) {
    if (!isCancelled()) reportOptionalError(error);
  }
}
