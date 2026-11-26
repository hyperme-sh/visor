import { useEffect, useState } from 'react';
import { appVersionLabel } from './native';

export function PhoneFooter() {
  const [version, setVersion] = useState('Loading version…');

  useEffect(() => {
    let active = true;
    void appVersionLabel().then(label => {
      if (active) setVersion(label);
    });
    return () => { active = false; };
  }, []);

  return <footer>
    <div>Private by default. Connected on your terms.</div>
    <div>{version}</div>
  </footer>;
}
