import { Alert, AlertDescription } from '../ui/alert';

export type ErrorBannerProps = {
  message: string | null;
};

export const ErrorBanner = ({ message }: ErrorBannerProps) => {
  if (!message) {
    return null;
  }
  return (
    <Alert className="font-chrome rounded-none border-x-0 text-xs" variant="destructive">
      <AlertDescription className="text-xs">{message}</AlertDescription>
    </Alert>
  );
};
