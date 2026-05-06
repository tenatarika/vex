export interface UserService {
  findById(id: string): Promise<User | null>;
  create(data: CreateUserDto): Promise<User>;
}

export class UserServiceImpl implements UserService {
  constructor(private readonly repo: UserRepository) {}

  async findById(id: string): Promise<User | null> {
    return this.repo.findOne(id);
  }

  async create(data: CreateUserDto): Promise<User> {
    const user = new User(data);
    return this.repo.save(user);
  }
}

export type UserId = string;

export const MAX_PAGE_SIZE = 100;

const formatUser = (user: User): UserDto => ({
  id: user.id,
  name: user.name,
  email: user.email,
});
