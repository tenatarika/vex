interface Animal {
  name: string;
}

class Dog implements Animal {
  name: string = "Rex";
}

class Cat implements Animal {
  name: string = "Whiskers";
}

function helper(): void {}
